//! Reducer/integration tests for `App`, split out of the monolithic `app.rs`.
//! Kept as one cohesive module so the shared test helpers (song builders, app
//! fixtures, Cmd extractors) stay in one place rather than scattered per domain.

use super::*;
use crossterm::event::{KeyEventKind, KeyEventState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn alt_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// The `af` chain set by a `SetAudioFilter` command among `cmds`, if any.
fn af(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::SetAudioFilter(s)) => Some(s.as_str()),
        _ => None,
    })
}

/// The URL of the `Load` command among `cmds`, if any. (A load now also emits
/// `SaveLibrary`, so tests look for the Load rather than an exact one-element match.)
fn load_url(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::Load(u)) => Some(u.as_str()),
        _ => None,
    })
}

fn has_stop(cmds: &[Cmd]) -> bool {
    cmds.iter()
        .any(|c| matches!(c, Cmd::Player(PlayerCmd::Stop)))
}

#[test]
fn q_is_back_in_player_mode_without_quitting() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_q_quits_in_player_mode() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn korean_q_key_is_back_without_quitting() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('ㅂ'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn korean_ctrl_q_key_quits_in_player_mode() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅂ'))));
    assert!(app.should_quit);
}

#[test]
fn korean_ctrl_c_still_quits() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅊ'))));
    assert!(app.should_quit);
}

#[test]
fn ctrl_a_selects_then_backspace_clears_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // open search (input focused)
    assert_eq!(app.mode, Mode::Search);
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.search.input, "lofi");
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.search.select_all);
    // Backspace with everything selected clears the field, not one char.
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.search.input, "");
    assert!(!app.search.select_all);
}

#[test]
fn ctrl_a_then_typing_replaces_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert_eq!(app.search.input, "x");
    assert!(!app.search.select_all);
}

#[test]
fn navigating_away_clears_a_pending_select_all_highlight() {
    let mut app = App::new(100);
    // Search box: select the whole query, then leave via Ctrl+H (a global nav action that's
    // resolved before the input handler's own deselect runs).
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.search.select_all);
    app.update(Msg::Key(ctrl(KeyCode::Char('h')))); // go home
    assert!(
        !app.search.select_all,
        "highlight must not survive leaving the screen"
    );

    // DJ Gem box: same story — select all, leave, flag cleared so it can't reappear highlighted.
    app.update(Msg::Key(key(KeyCode::Char('g')))); // enter DJ Gem
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.ai.select_all);
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert!(!app.ai.select_all);
}

#[test]
fn ctrl_a_selects_then_backspace_clears_ai_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('g')))); // open DJ Gem assistant (input focused)
    assert_eq!(app.mode, Mode::Ai);
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.ai.input, "hi");
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.ai.select_all);
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.ai.input, "");
    assert!(!app.ai.select_all);
}

#[test]
fn radio_extend_resumes_playback_when_idle() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = None; // the seed ended before this refill landed
    let cmds = app.extend_queue_from_radio(vec![Song::remote("b", "B", "y", "2:00")]);
    assert!(
        load_url(&cmds).is_some(),
        "should resume by loading the new track"
    );
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("b"));
}

#[test]
fn radio_extend_prefetches_next_while_playing() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = Some("a".to_owned()); // still playing the seed
    let cmds = app.extend_queue_from_radio(vec![Song::remote("b", "B", "y", "2:00")]);
    assert!(
        load_url(&cmds).is_none(),
        "must not interrupt the playing track"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Resolve { video_id, .. } if video_id == "b")),
        "should prefetch the upcoming track's stream"
    );
}

fn confirm_on_f5_keymap() -> KeyMap {
    let mut keymap = KeyMap::default();
    keymap
        .rebind(
            KeyContext::Common,
            Action::Confirm,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();
    keymap
}

#[test]
fn space_toggles_pause_and_emits_cmd() {
    let mut app = App::new(100);
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
    assert!(app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));
}

#[test]
fn restores_last_history_track_without_autoplaying() {
    let mut app = App::new(100);
    app.library.record_play(&songs(2)[0]);
    app.library.record_play(&songs(2)[1]);
    app.restore_last_played_from_library();
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id1");
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
}

#[test]
fn play_loads_restored_history_track() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
    assert!(
        load_url(&cmds)
            .expect("restored track load")
            .contains("id0")
    );
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    assert!(!app.playback.paused);
}

#[test]
fn autoplay_on_start_plays_restored_track_when_enabled() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    app.config.autoplay_on_start = Some(true);
    // The launch trigger loads the restored track and starts it (no key press).
    let cmds = app.update(Msg::Autoplay);
    assert!(
        load_url(&cmds)
            .expect("autoplay load at launch")
            .contains("id0")
    );
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    assert!(!app.playback.paused);
}

#[test]
fn autoplay_on_start_is_noop_when_disabled() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    // Default (opt-in off): the trigger does nothing; the track stays paused and unloaded.
    assert!(!app.config.effective_autoplay_on_start());
    let cmds = app.update(Msg::Autoplay);
    assert!(load_url(&cmds).is_none());
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(app.playback.paused);
}

#[test]
fn up_down_adjust_volume_in_player_mode() {
    let mut app = App::new(50);
    let cmds = app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.playback.volume, 55);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(55))]
    ));

    let cmds = app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.playback.volume, 50);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(50))]
    ));
}

#[test]
fn time_pos_redraws_only_on_second_change() {
    let mut app = App::new(100);
    app.dirty = false;
    app.update(Msg::PlayerTimePos(1.1));
    assert!(app.dirty);
    app.dirty = false;
    app.update(Msg::PlayerTimePos(1.9)); // same whole second
    assert!(!app.dirty);
    app.update(Msg::PlayerTimePos(2.0)); // new second
    assert!(app.dirty);
}

#[test]
fn s_enters_search_and_q_is_typed_not_quit() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.should_quit);
    assert_eq!(app.search.input, "q");
}

#[test]
fn korean_letters_still_type_in_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    app.update(Msg::Key(key(KeyCode::Char('ㅂ'))));
    assert!(!app.should_quit);
    assert_eq!(app.search.input, "ㅂ");
}

#[test]
fn korean_shortcut_key_redraws_even_when_unhandled() {
    let mut app = App::new(100);
    app.dirty = false;
    app.update(Msg::Key(key(KeyCode::Char('ㅛ'))));
    assert!(app.dirty);
}

#[test]
fn ime_preedit_scrub_is_disabled_in_text_entry() {
    let mut app = App::new(100);
    assert!(app.should_scrub_ime_preedit());
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert!(!app.should_scrub_ime_preedit());
}

#[test]
fn enter_in_search_emits_search_cmd() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [
            Cmd::Search {
                query,
                source,
                config,
            },
        ] => {
            assert_eq!(query, "lofi");
            assert_eq!(*source, SearchSource::Youtube);
            assert_eq!(config.source, SearchSource::Youtube);
        }
        _ => panic!("expected a Search cmd"),
    }
}

#[test]
fn tab_opens_search_source_menu_and_cycles_source() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));

    let cmds = app.update(Msg::Key(key(KeyCode::Tab)));
    assert!(cmds.is_empty());
    assert!(app.dropdowns.search_source_open);
    assert_eq!(app.search.source, SearchSource::Youtube);

    let cmds = app.update(Msg::Key(key(KeyCode::Down)));
    assert!(app.dropdowns.search_source_open);
    assert_eq!(app.search.source, SearchSource::SoundCloud);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::SaveConfig(cfg)] if cfg.search.source == SearchSource::SoundCloud
    ));

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert!(!app.dropdowns.search_source_open);
}

#[test]
fn shift_tab_toggles_search_focus_between_input_and_results() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(1);
    app.search.focus = SearchFocus::Results;
    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.search.focus, SearchFocus::Input);

    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn remapped_search_focus_toggle_updates_both_directions() {
    let mut app = App::new(100);
    app.keymap
        .rebind(
            KeyContext::SearchResults,
            Action::FocusPrev,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();

    app.mode = Mode::Search;
    app.search.results = songs(1);
    app.search.focus = SearchFocus::Results;
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.search.focus, SearchFocus::Input);

    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn search_submit_stays_enter_when_common_confirm_is_remapped() {
    let mut app = App::new(100);
    app.keymap = confirm_on_f5_keymap();
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert!(cmds.is_empty());
    assert!(!app.search.searching);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [Cmd::Search { query, source, .. }] => {
            assert_eq!(query, "lofi");
            assert_eq!(*source, SearchSource::Youtube);
        }
        _ => panic!("expected a Search cmd"),
    }
}

#[test]
fn search_enter_beats_enter_global_remap_but_other_screens_keep_it() {
    let mut keymap = confirm_on_f5_keymap();
    keymap
        .rebind(
            KeyContext::Global,
            Action::ToggleHelp,
            crate::keymap::parse_chord("enter").unwrap(),
        )
        .unwrap();

    let mut app = App::new(100);
    app.keymap = keymap.clone();
    app.mode = Mode::Search;
    app.search.input = "lofi".to_owned();
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.help_visible);
    match cmds.as_slice() {
        [Cmd::Search { query, source, .. }] => {
            assert_eq!(query, "lofi");
            assert_eq!(*source, SearchSource::Youtube);
        }
        _ => panic!("expected a Search cmd"),
    }

    let mut player = App::new(100);
    player.keymap = keymap;
    assert!(player.update(Msg::Key(key(KeyCode::Enter))).is_empty());
    assert!(player.help_visible);
}

#[test]
fn results_then_enter_plays_and_returns_to_player() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        songs: vec![Song::remote("abc123", "Song", "Artist", "3:00")],
    });
    assert_eq!(app.search.focus, SearchFocus::Results);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains("abc123"));
}

#[test]
fn enter_on_search_result_queues_only_the_selected_song() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        songs: vec![
            Song::remote("id0", "Zero", "A", "3:00"),
            Song::remote("id1", "One", "B", "3:00"),
            Song::remote("id2", "Two", "C", "3:00"),
        ],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 1;
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    // Only the picked track lands in the queue — not the whole result list. Nothing was
    // playing, so it starts immediately.
    assert_eq!(app.queue.len(), 1);
    assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
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
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        songs: vec![Song::remote("new9", "New", "Z", "3:00")],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 0;
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    // The picked track starts playing immediately and we jump to the Player…
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains("new9"));
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
        query: "x".to_owned(),
        source: SearchSource::Youtube,
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
    assert!(load_url(&cmds).is_none());
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
    assert!(load_url(&cmds).is_none());
    assert_eq!(app.mode, Mode::Search);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains("id0"));
}

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
    assert!(load_url(&cmds).is_some());
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
    assert!(load_url(&cmds).is_none());
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert_eq!(app.queue.len(), before_len + 1);
    assert!(app.queue.video_ids().any(|v| v == target.as_str()));
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
    assert!(load_url(&cmds).expect("a Load cmd").contains("f0"));
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

    assert!(load_url(&cmds).is_none());
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
    assert!(load_url(&cmds).is_some());
    assert_eq!(app.queue.len(), rows);
    assert!(app.queue.video_ids().all(|v| v != "id0" && v != "id1"));
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("f1"));
}

#[test]
fn right_click_on_a_search_row_adds_it_to_the_queue() {
    // Something is already playing, so the right-click must not interrupt it.
    let mut app = app_playing(2, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        songs: vec![
            Song::remote("r0", "R0", "A", "3:00"),
            Song::remote("r1", "R1", "B", "3:00"),
        ],
    });
    app.search.focus = SearchFocus::Results;

    // Render so the per-row hit rects are published, then right-click row 1.
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row1 = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::ListRow(1))
        .map(|b| b.rect)
        .expect("a rendered search row rect");

    let cmds = app.update(Msg::MouseRightClick {
        col: row1.x,
        row: row1.y,
    });
    // The row got selected and enqueued without interrupting playback.
    assert_eq!(app.search.selected, 1);
    assert!(load_url(&cmds).is_none());
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert!(app.queue.video_ids().any(|v| v == "r1"));
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn q_closes_search_results_without_quitting_app() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(1);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_q_quits_from_search_results() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(1);
    app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn ctrl_h_goes_home_from_search_input_without_typing() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    app.search.input = "abc".to_owned();
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.search.input, "abc");
    assert!(!app.should_quit);
}

#[test]
fn korean_ctrl_h_goes_home_from_library() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅗ'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_h_goes_home_from_help_overlay() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.help_visible = true;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.help_visible);
    assert!(!app.should_quit);
}

// --- M4: queue / shuffle / repeat / auto-advance ------------------------

fn songs(n: usize) -> Vec<Song> {
    (0..n)
        .map(|i| Song::remote(format!("id{i}"), format!("t{i}"), "a", "0:10"))
        .collect()
}

fn radio_station(id: &str) -> Song {
    Song::from_source(
        SearchSource::RadioBrowser,
        id,
        format!("Station {id}"),
        "KR / MP3",
        "",
        crate::api::PlayableRef::RadioStream {
            url: format!("https://example.com/{id}.mp3"),
        },
    )
}

/// An app with an `n`-track queue, playing track `start`. Builds the queue directly so
/// it stays independent of how individual play paths populate the queue (e.g. search-play
/// only queues the one picked track).
fn app_playing(n: usize, start: usize) -> App {
    let mut app = App::new(100);
    app.queue.set(songs(n), start);
    app.mode = Mode::Player;
    let song = app.queue.current().cloned();
    app.load_song(song);
    app
}

fn current(app: &App) -> &str {
    app.queue.current().unwrap().video_id.as_str()
}

#[test]
fn eof_auto_advances_to_next_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::PlayerEof);
    assert!(load_url(&cmds).expect("load of next track").contains("id1"));
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_at_end_with_repeat_off_stops() {
    let mut app = app_playing(2, 1); // already on the last track
    let cmds = app.update(Msg::PlayerEof);
    // Playback stops (no load/advance), though the finished track is still recorded.
    assert!(load_url(&cmds).is_none());
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_with_repeat_one_replays_same_track() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    let cmds = app.update(Msg::PlayerEof);
    assert!(
        load_url(&cmds)
            .expect("replay of same track")
            .contains("id0")
    );
    assert_eq!(current(&app), "id0");
}

#[test]
fn player_error_auto_skips_to_next_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::PlayerError("boom".to_owned()));
    // The unplayable track is skipped: cursor + title move to the next track.
    assert!(load_url(&cmds).expect("load of next track").contains("id1"));
    assert_eq!(current(&app), "id1");
    assert!(app.status.text.contains("skipped") || app.status.text.contains("unavailable"));
}

#[test]
fn player_error_stops_after_repeated_failures() {
    let mut app = app_playing(6, 0);
    // First MAX failures auto-skip...
    for _ in 0..MAX_CONSECUTIVE_PLAY_ERRORS {
        let cmds = app.update(Msg::PlayerError("boom".to_owned()));
        assert!(load_url(&cmds).is_some(), "still skipping within budget");
    }
    // ...the next one gives up instead of skip-storming the whole queue.
    let cmds = app.update(Msg::PlayerError("boom".to_owned()));
    assert!(load_url(&cmds).is_none(), "stops skipping after the budget");
    assert!(app.status.text.contains("stopped") || app.status.text.contains("failed"));
}

#[test]
fn successful_playback_resets_the_error_streak() {
    let mut app = app_playing(5, 0);
    app.update(Msg::PlayerError("boom".to_owned())); // skip to id1 (streak = 1)
    assert_eq!(current(&app), "id1");
    app.update(Msg::PlayerTimePos(3.0)); // id1 actually plays → streak cleared
    // A later failure starts a fresh streak, so it skips again rather than giving up.
    let cmds = app.update(Msg::PlayerError("boom".to_owned()));
    assert!(
        load_url(&cmds)
            .expect("skips again after a clean play")
            .contains("id2")
    );
    assert_eq!(current(&app), "id2");
}

#[test]
fn n_advances_and_p_goes_back() {
    let mut app = app_playing(3, 0);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert_eq!(current(&app), "id1");
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert_eq!(current(&app), "id0");
}

#[test]
fn delete_on_player_removes_current_and_loads_next() {
    let mut app = app_playing(3, 0);

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id1");
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id0"),
        "deleted current track should be removed from the queue"
    );
    assert!(load_url(&cmds).expect("load of next track").contains("id1"));
}

#[test]
fn r_cycles_repeat_and_persists() {
    let mut app = app_playing(3, 0);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.repeat, crate::queue::Repeat::All);
}

#[test]
fn s_enters_search_and_shift_s_toggles_shuffle() {
    let mut app = app_playing(3, 0);
    assert!(!app.queue.shuffle);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    assert!(!app.queue.shuffle);

    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('S'))));
    assert!(app.queue.shuffle);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.shuffle, Some(true));
    // Shuffle keeps the current track current.
    assert_eq!(current(&app), "id0");
}

// --- B+C: EQ / normalize / speed / autoplay -----------------------------

#[test]
fn e_cycles_eq_preset_and_emits_filter() {
    let mut app = app_playing(3, 0);
    assert_eq!(app.audio.preset, EqPreset::Flat);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert_eq!(app.audio.preset, EqPreset::BassBoost);
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("equalizer")
    );
    // Cycle the rest of the way back to Flat → the chain is cleared (empty string).
    let mut last = Vec::new();
    for _ in 0..(EqPreset::CYCLE.len() - 1) {
        last = app.update(Msg::Key(key(KeyCode::Char('e'))));
    }
    assert_eq!(app.audio.preset, EqPreset::Flat);
    assert_eq!(af(&last), Some(""));
}

#[test]
fn shift_n_toggles_normalization() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
    assert!(app.audio.normalize);
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("dynaudnorm")
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
    assert!(!app.audio.normalize);
    assert_eq!(af(&cmds), Some(""));
}

#[test]
fn speed_up_and_down_clamp_and_emit() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('>'))));
    assert!((app.playback.speed - 1.1).abs() < 1e-9);
    assert!(cmds.iter().any(|c| matches!(c,
        Cmd::Player(PlayerCmd::SetProperty { name, .. }) if name == "speed")));
    // Floor at SPEED_MIN no matter how many times we press down.
    for _ in 0..50 {
        app.update(Msg::Key(key(KeyCode::Char('<'))));
    }
    assert!((app.playback.speed - SPEED_MIN).abs() < 1e-9);
}

#[test]
fn ctrl_r_toggles_autoplay_radio() {
    let mut app = app_playing(3, 0);
    assert!(!app.autoplay_radio);
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_radio);
    assert_eq!(
        save_config(&cmds).expect("a SaveConfig cmd").autoplay_radio,
        Some(true)
    );
    // Plain `r` still cycles repeat (not the autoplay toggle).
    app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(app.autoplay_radio);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_radio);
    assert_eq!(
        save_config(&cmds).expect("a SaveConfig cmd").autoplay_radio,
        Some(false)
    );
}

#[test]
fn alt_shift_r_confirms_dedicated_radio_mode() {
    let mut app = app_playing(1, 0);
    assert!(!app.radio_dedicated_mode);
    assert!(
        !app.search_config_for_mode()
            .selectable_sources()
            .contains(&SearchSource::RadioBrowser)
    );
    assert_eq!(app.library_tabs(), &LibraryTab::NORMAL);

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(cmds.is_empty());
    assert_eq!(
        app.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
    assert!(!app.radio_dedicated_mode);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.radio_dedicated_mode);
    assert!(app.pending_radio_mode_confirm.is_none());
    assert_eq!(app.theme.preset, "dario");
    assert_eq!(
        app.theme.effective_hex(crate::theme::ThemeRole::Background),
        "none"
    );
    assert_eq!(
        app.search_config_for_mode().selectable_sources(),
        vec![SearchSource::RadioBrowser]
    );
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert_eq!(app.library_tabs(), &LibraryTab::RADIO_MODE);

    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai, "DJ Gem remains available in Radio mode");
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);

    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert_eq!(app.pending_radio_mode_confirm, Some(RadioModeConfirm::Exit));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "default");
    assert!(
        !app.search_config_for_mode()
            .selectable_sources()
            .contains(&SearchSource::RadioBrowser)
    );
}

#[test]
fn alt_shift_r_radio_mode_switch_only_works_on_player() {
    let mut app = App::new(100);

    app.mode = Mode::Search;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(app.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Library;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(app.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Player;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert_eq!(
        app.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

#[test]
fn radio_mode_switch_stops_playback_restores_cached_queues_and_themes() {
    let mut app = app_playing(3, 1);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    app.playback.paused = false;
    app.radio.pending = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "id1".to_owned(),
        shortlist: Vec::new(),
        local_pick: Vec::new(),
        cid_map: Vec::new(),
        mode: crate::radio::config::RadioMode::Balanced,
        cache_key: 42,
    });

    let enter = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    assert!(app.radio_dedicated_mode);
    assert!(has_stop(&enter), "entering Radio mode should stop mpv");
    assert!(app.queue.is_empty());
    assert!(app.playback.paused);
    assert!(load_url(&enter).is_none());
    assert!(!app.radio.pending);
    assert!(app.radio.pending_rerank.is_none());
    assert_eq!(app.theme.preset, "dario");

    app.queue.set(
        vec![radio_station("station-a"), radio_station("station-b")],
        1,
    );
    app.load_song(app.queue.current().cloned());
    app.playback.paused = false;
    app.theme.set_preset(crate::theme::ThemePreset::RosePine);
    let exit = app.apply_radio_mode_confirm(RadioModeConfirm::Exit);

    assert!(!app.radio_dedicated_mode);
    assert!(has_stop(&exit), "leaving Radio mode should stop mpv");
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id1");
    assert!(
        load_url(&exit)
            .expect("restored normal track load")
            .contains("id1")
    );
    assert!(!app.playback.paused);
    assert_eq!(app.theme.preset, "midnight");

    app.theme.set_preset(crate::theme::ThemePreset::Light);
    app.queue.set(songs(2), 0);
    app.load_song(app.queue.current().cloned());
    let reenter = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    assert!(app.radio_dedicated_mode);
    assert!(has_stop(&reenter));
    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "rad:station-b");
    assert!(
        load_url(&reenter)
            .expect("restored Radio station load")
            .contains("station-b.mp3")
    );
    assert!(!app.playback.paused);
    assert_eq!(
        app.theme.preset, "rose_pine",
        "Radio mode should remember the last Radio theme"
    );

    let second_exit = app.apply_radio_mode_confirm(RadioModeConfirm::Exit);

    assert!(!app.radio_dedicated_mode);
    assert!(has_stop(&second_exit));
    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id0");
    assert!(
        load_url(&second_exit)
            .expect("updated normal queue load")
            .contains("id0")
    );
    assert_eq!(app.theme.preset, "light");
}

#[test]
fn radio_mode_theme_edits_do_not_overwrite_normal_config_theme() {
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    app.open_settings();
    {
        let st = app.settings.as_mut().expect("settings open");
        st.draft
            .theme
            .set_preset(crate::theme::ThemePreset::RosePine);
    }
    let cmds = app.close_settings();

    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))));
    assert_eq!(app.theme.preset, "rose_pine");
    assert_eq!(
        app.config.theme.preset, "midnight",
        "normal theme in config should survive Radio-mode theme edits"
    );

    app.apply_radio_mode_confirm(RadioModeConfirm::Exit);
    assert_eq!(app.theme.preset, "midnight");
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert_eq!(app.theme.preset, "rose_pine");
}

#[test]
fn radio_mode_nav_labels_player_as_radio_without_shifting_tabs() {
    let mut app = App::new(100);

    let normal = render_app_buffer(&app, 80, 24);
    let normal_text: String = normal
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        normal_text.contains("Player"),
        "normal nav should show Player"
    );
    let normal_buttons = app.bridges.mouse_buttons.borrow();
    let normal_player = normal_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Player))
        .expect("normal Player tab")
        .rect;
    let normal_search = normal_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .expect("normal Search tab")
        .rect;
    drop(normal_buttons);

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        radio_text.contains("Radio"),
        "Radio nav should replace Player"
    );
    let radio_buttons = app.bridges.mouse_buttons.borrow();
    let radio_player = radio_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Player))
        .expect("Radio Player tab")
        .rect;
    let radio_search = radio_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .expect("Radio Search tab")
        .rect;

    assert_eq!(radio_player.width, normal_player.width);
    assert_eq!(
        radio_search.x, normal_search.x,
        "Search tab should not shift when Player becomes Radio"
    );
    assert!(
        radio_buttons
            .iter()
            .any(|b| b.target == MouseTarget::Nav(Mode::Ai)),
        "DJ Gem tab stays visible in Radio mode"
    );
}

#[test]
fn double_clicking_active_player_tab_confirms_radio_mode() {
    let mut app = App::new(100);

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Player));

    assert!(cmds.is_empty());
    assert_eq!(
        app.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

#[test]
fn autoplay_radio_does_not_extend_from_radio_browser_streams() {
    let mut app = App::new(100);
    app.autoplay_radio = true;
    app.queue.set(vec![radio_station("station-seed")], 0);
    app.mode = Mode::Player;

    let cmds = app.load_song(app.queue.current().cloned());

    assert!(!cmds.iter().any(|c| matches!(c, Cmd::RadioFallback { .. })));
    assert!(app.library.history.is_empty());
    assert_eq!(app.library.radios.len(), 1);
}

#[test]
fn load_song_reapplies_active_eq_chain() {
    let mut app = app_playing(3, 0);
    app.audio.bands = EqPreset::BassBoost.gains();
    // A manual skip reloads the track and must re-send the EQ chain (gapless rebuild
    // can otherwise drop the labeled bands).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("equalizer")
    );
}

#[test]
fn apply_config_pushes_playback_settings() {
    let cfg = crate::config::Config {
        eq_preset: EqPreset::Vocal,
        normalize: Some(true),
        speed: Some(1.5),
        seek_seconds: Some(30.0),
        shuffle: Some(true),
        repeat: crate::queue::Repeat::One,
        autoplay_radio: Some(true),
        ..crate::config::Config::default()
    };
    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.audio.preset, EqPreset::Vocal);
    assert_eq!(app.audio.bands, EqPreset::Vocal.gains());
    assert!(app.audio.normalize);
    assert!((app.playback.speed - 1.5).abs() < 1e-9);
    assert!((app.audio.seek_seconds - 30.0).abs() < 1e-9);
    assert!(app.queue.shuffle);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::One);
    assert!(app.autoplay_radio);
}

#[test]
fn seek_keys_use_the_configured_interval() {
    let mut app = app_playing(1, 0);
    app.apply_config(&crate::config::Config {
        seek_seconds: Some(30.0),
        ..Default::default()
    });
    // Forward (→) jumps +interval, backward (←) jumps −interval.
    match app.update(Msg::Key(key(KeyCode::Right))).as_slice() {
        [Cmd::Player(PlayerCmd::SeekRelative(s))] => assert!((*s - 30.0).abs() < 1e-9),
        _ => panic!("expected a single SeekRelative(+30) cmd"),
    }
    match app.update(Msg::Key(key(KeyCode::Left))).as_slice() {
        [Cmd::Player(PlayerCmd::SeekRelative(s))] => assert!((*s + 30.0).abs() < 1e-9),
        _ => panic!("expected a single SeekRelative(-30) cmd"),
    }
}

// --- D: settings screen -------------------------------------------------

fn save_config(cmds: &[Cmd]) -> Option<&Config> {
    cmds.iter().find_map(|c| match c {
        Cmd::SaveConfig(c) => Some(c.as_ref()),
        _ => None,
    })
}

#[test]
fn losing_terminal_focus_parks_animations_then_regaining_resumes() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;
    // Master + one effect on → animations are logically running.
    app.config.animations.master = true;
    app.config.animations.rain = true;
    assert!(
        app.config.animations.pause_unfocused,
        "pause_unfocused defaults on"
    );
    // Focused by default (the safe state for terminals that never report focus) → clock runs.
    assert!(app.focused);
    assert!(app.animation_active());
    // Losing focus (window minimized / behind another) parks the ~fps tick...
    app.update(Msg::Focus(false));
    assert!(!app.focused);
    assert!(!app.animation_active());
    // ...and regaining it resumes immediately.
    app.update(Msg::Focus(true));
    assert!(app.animation_active());
    // Opting out keeps animating even while unfocused.
    app.config.animations.pause_unfocused = false;
    app.update(Msg::Focus(false));
    assert!(
        app.animation_active(),
        "pause_unfocused=false should keep animating unfocused"
    );
}

#[test]
fn overlays_do_not_park_animations_but_focus_still_does() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.rain = true;

    assert!(app.animation_active());

    app.help_visible = true;
    assert!(
        app.animation_active(),
        "cheat-sheet overlay should not pause the background animation"
    );
    app.help_visible = false;

    app.about_visible = true;
    assert!(
        app.animation_active(),
        "About overlay should not pause the background animation"
    );
    app.about_visible = false;

    app.why_ai_visible = true;
    assert!(
        app.animation_active(),
        "Why-DJ Gem overlay should not pause the background animation"
    );

    app.update(Msg::Focus(false));
    assert!(
        !app.animation_active(),
        "focus loss still parks animations even while an overlay is visible"
    );
}

#[test]
fn canvas_animation_advances_phase_every_tick_but_caps_redraws() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.rain = true;
    app.config.animations.fps = 30;

    assert_eq!(app.animation_tick_fps(), 30);
    assert_eq!(app.animation_draw_fps(), 20);

    let mut redraws = 0;
    for expected_frame in 1..=30 {
        app.dirty = false;
        app.update(Msg::AnimTick);
        assert_eq!(app.anim_frame, expected_frame);
        redraws += usize::from(app.dirty);
    }
    assert_eq!(redraws, 20);
}

#[test]
fn ai_mascot_animation_redraws_only_when_pose_can_change() {
    let mut app = app_playing(1, 0);
    app.mode = Mode::Ai;
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.fps = 30;

    assert!(app.animation_active());
    assert_eq!(app.animation_tick_fps(), 30);
    assert_eq!(app.animation_draw_fps(), 3);

    let mut redraws = 0;
    for _ in 0..30 {
        app.dirty = false;
        app.update(Msg::AnimTick);
        redraws += usize::from(app.dirty);
    }
    assert_eq!(redraws, 3);
}

#[test]
fn toggling_animations_while_settings_open_survives_close() {
    let mut app = app_playing(1, 0);
    // Open settings; the draft is seeded from the (off) live config.
    app.update(Msg::Key(key(KeyCode::Char(','))));
    assert!(app.settings.is_some());
    assert!(!app.config.animations.master);
    // Toggle via the shared path (what both the `A` key and the ✨ click call).
    let cmds = app.toggle_animations();
    assert!(app.config.animations.master);
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))),
        "toggle persists"
    );
    // The draft must mirror the flip; otherwise close commits the stale (off) draft over it.
    assert!(app.settings.as_ref().unwrap().draft.animations.master);
    // Closing settings commits the draft → config; the toggle must stick, not revert.
    app.close_settings();
    assert!(
        app.config.animations.master,
        "close_settings must not revert the toggle"
    );
}

#[test]
fn comma_opens_settings_and_q_closes_without_quitting() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(','))));
    assert_eq!(app.mode, Mode::Settings);
    assert!(app.settings.is_some());
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
    assert!(app.settings.is_none());
}

#[test]
fn settings_tab_cycles_through_all_tabs() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Playback);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Graphics);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General); // wraps
}

#[test]
fn settings_keys_lists_radio_normal_mode_binding() {
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    app.settings.as_mut().unwrap().tab = SettingsTab::Keys;

    let buf = render_app_buffer(&app, 120, 40);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("Player"),
        "Keys tab should show player bindings"
    );
    assert!(
        text.contains("Radio/Normal mode"),
        "Keys tab should list the mode-switch action"
    );
    assert!(
        text.contains("Alt+Shift+R"),
        "Keys tab should show the default mode-switch key"
    );
}

#[test]
fn help_overlay_shows_player_radio_normal_mode_binding() {
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.help_visible = true;

    let buf = render_app_buffer(&app, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("Radio/Normal mode"),
        "Help should show the player-only mode-switch action"
    );
    assert!(
        text.contains("Alt+Shift+R"),
        "Help should show the default mode-switch key"
    );
}

#[test]
fn remapped_focus_keys_switch_library_and_settings_tabs() {
    let mut app = app_playing(1, 0);
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::FocusNext,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::FocusPrev,
            crate::keymap::parse_chord("f6").unwrap(),
        )
        .unwrap();

    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app.update(Msg::Key(key(KeyCode::F(6))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::All);

    app.update(Msg::Key(key(KeyCode::Char('q'))));
    app.update(Msg::Key(key(KeyCode::Char(','))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Playback);
    app.update(Msg::Key(key(KeyCode::F(6))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
}

#[test]
fn transient_status_expires_after_ttl_and_restores_the_title() {
    let mut app = app_playing(1, 0);
    // A notification covers the title and arms the expiry timer.
    app.update(Msg::Key(key(KeyCode::Char('N')))); // toggle normalize → sets status
    assert!(!app.status.text.is_empty(), "an action should set a status");
    assert!(
        app.status_visible(),
        "a non-empty status arms the expiry tick"
    );

    // Before the TTL elapses, a tick is a no-op — the notification stays.
    app.update(Msg::StatusTick);
    assert!(
        !app.status.text.is_empty(),
        "status persists until the TTL elapses"
    );
    assert!(app.status_visible());

    // Backdate the timer past the TTL; the next tick clears it and restores the title.
    app.status.set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    app.dirty = false; // so the assertion below proves the clear requested the redraw
    app.update(Msg::StatusTick);
    assert!(
        app.status.text.is_empty(),
        "status auto-clears after the TTL"
    );
    assert!(!app.status_visible(), "expiry disarms the tick");
    assert!(
        app.dirty,
        "clearing the status requests a redraw of the title"
    );
}

#[test]
fn radio_mode_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    use crate::radio::RadioMode;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Fields: AiEnabled(0), Model(1), ApiKey(2), RomanizedTitles(3), Clear cache(4),
    // AutoplayRadio(5), RadioMode(6).
    for _ in 0..6 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right))); // Balanced → Discovery
    assert_eq!(
        app.settings.as_ref().unwrap().draft.radio_mode,
        RadioMode::Discovery
    );
    assert!(app.status.text.contains("Radio mode: Discovery"));
    // Closing settings commits the draft into config + emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.radio.mode, RadioMode::Discovery);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))));
}

#[test]
fn clear_romanized_title_cache_button_is_hidden_in_retro_draft() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }

    let st = app.settings.as_ref().unwrap();
    assert_eq!(st.tab, SettingsTab::Ai);
    assert!(st.fields().contains(&Field::ClearRomanizedTitleCache));

    app.settings.as_mut().unwrap().draft.retro_mode = true;
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .fields()
            .contains(&Field::ClearRomanizedTitleCache)
    );
}

#[test]
fn clear_romanized_title_cache_confirms_and_discards_stale_results() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.config.romanized_titles = Some(true);
    app.romanization.next_request_id = 7;
    let song = Song::remote("ko1", "좋은 날", "아이유", "0:10");
    let stale_key = crate::romanize::key_for_song(&song);
    assert!(app.romanization.cache.ensure_local(&song));
    assert!(app.romanization.cache.entry_for(&song).is_some());

    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    let idx = app
        .settings
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| *f == Field::ClearRomanizedTitleCache)
        .expect("clear cache field");
    app.settings.as_mut().unwrap().row = idx;

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(
        app.pending_settings_confirm,
        Some(SettingsConfirm::ClearRomanizedTitleCache)
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.pending_settings_confirm.is_none());
    assert_eq!(app.status.text, "Romanized title cache cleared");
    assert!(app.romanization.cache.entry_for(&song).is_none());
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::ClearRomanizedTitles))
    );

    let cmds = app.apply_romanized_titles(
        7,
        vec![stale_key.clone()],
        vec![crate::romanize::RomanizedResult {
            key: stale_key,
            title: "Joeun Nal".to_owned(),
            artist: "IU".to_owned(),
            confidence: Some(0.9),
        }],
    );
    assert!(cmds.is_empty());
    assert!(app.romanization.cache.entry_for(&song).is_none());
}

#[test]
fn settings_key_capture_accepts_ctrl_chords() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Hotkeys tab (index 2)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
    app.update(Msg::Key(key(KeyCode::Enter))); // capture first binding: player.toggle_pause
    assert_eq!(
        app.settings.as_ref().unwrap().capturing,
        Some((KeyContext::Player, Action::TogglePause))
    );
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅌ'))));
    assert_eq!(
        app.settings.as_ref().unwrap().keymap.action(
            KeyContext::Player,
            crate::keymap::parse_chord("ctrl+x").unwrap()
        ),
        Some(Action::TogglePause)
    );
    assert!(app.status.text.contains("^x"));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(
        saved
            .keybindings
            .get("player.toggle_pause")
            .map(String::as_str),
        Some("ctrl+x")
    );
}

#[test]
fn settings_key_capture_conflict_raises_modal_warning() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Hotkeys tab (index 2)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
    app.update(Msg::Key(key(KeyCode::Enter))); // capture player.toggle_pause

    // `q` is already Back in Player → a conflict warning pops instead of silently
    // dropping the rebind, and it names the offending chord, action, and context.
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    let conflict = app
        .key_conflict
        .expect("a conflict warning should be raised");
    assert_eq!(conflict.existing, Action::Back);
    assert_eq!(conflict.ctx, KeyContext::Player);
    assert_eq!(conflict.chord, crate::keymap::parse_chord("q").unwrap());
    // The binding was left untouched: space still toggles pause, `q` still means Back.
    let km = &app.settings.as_ref().unwrap().keymap;
    assert_eq!(
        km.action(
            KeyContext::Player,
            crate::keymap::parse_chord("space").unwrap()
        ),
        Some(Action::TogglePause)
    );
    assert_eq!(
        km.action(KeyContext::Player, crate::keymap::parse_chord("q").unwrap()),
        Some(Action::Back)
    );

    // The popup is modal: the next key only dismisses it (here `q` does NOT save+quit).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(app.key_conflict.is_none());
    assert!(
        save_config(&cmds).is_none(),
        "dismiss key must be swallowed, not saved"
    );
    assert!(app.settings.is_some(), "settings stayed open after dismiss");
}

/// Move the General-tab cursor onto the Reset-all button.
fn focus_reset_all(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General tab)
    for _ in 0..SettingsTab::General.fields().len() - 1 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::ResetAll)
    );
}

/// Move the General-tab cursor onto the Reset-keybindings button.
fn focus_reset_keybindings(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General tab)
    let idx = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ResetKeybindings)
        .expect("reset keybindings field");
    for _ in 0..idx {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::ResetKeybindings)
    );
}

#[test]
fn reset_keybindings_button_restores_defaults_and_persists_on_close() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.keymap
        .rebind(
            KeyContext::Player,
            Action::TogglePause,
            crate::keymap::parse_chord("P").unwrap(),
        )
        .unwrap();
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("P").unwrap()),
        Some(Action::TogglePause)
    );

    focus_reset_keybindings(&mut app);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(
        app.pending_settings_confirm,
        Some(SettingsConfirm::ResetKeybindings)
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(cmds.is_empty());
    assert_eq!(app.status.text, "Keybindings reset to defaults");

    let draft_keymap = &app.settings.as_ref().unwrap().keymap;
    assert_eq!(
        draft_keymap.action(
            KeyContext::Player,
            crate::keymap::parse_chord("space").unwrap()
        ),
        Some(Action::TogglePause)
    );
    assert_eq!(
        draft_keymap.action(KeyContext::Player, crate::keymap::parse_chord("P").unwrap()),
        None
    );
    // The live keymap follows the existing Settings flow: changes commit on close.
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("P").unwrap()),
        Some(Action::TogglePause)
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert!(saved.keybindings.is_empty());
    assert_eq!(
        app.keymap.action(
            KeyContext::Player,
            crate::keymap::parse_chord("space").unwrap()
        ),
        Some(Action::TogglePause)
    );
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("P").unwrap()),
        None
    );
}

#[test]
fn reset_all_button_confirms_then_restores_defaults() {
    let mut app = app_playing(1, 0);
    focus_reset_all(&mut app);
    // Dirty several draft values across tabs.
    {
        let d = &mut app.settings.as_mut().unwrap().draft;
        d.speed = 1.8;
        d.seek_seconds = 45.0;
        d.gemini_api_key = "AIzaSecret".to_owned();
    }
    // Enter opens the confirmation modal (does not reset yet).
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(
        app.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
    // `y` confirms → every draft value is back to its default.
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.pending_settings_confirm.is_none());
    let d = &app.settings.as_ref().unwrap().draft;
    assert!((d.speed - 1.0).abs() < 1e-9);
    assert!((d.seek_seconds - 10.0).abs() < 1e-9);
    assert!(d.gemini_api_key.is_empty());
}

#[test]
fn reset_all_button_cancel_leaves_settings_untouched() {
    let mut app = app_playing(1, 0);
    focus_reset_all(&mut app);
    app.settings.as_mut().unwrap().draft.speed = 1.8;
    app.update(Msg::Key(key(KeyCode::Enter))); // open modal
    assert_eq!(
        app.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    app.update(Msg::Key(key(KeyCode::Esc))); // anything but Enter/`y` cancels
    assert!(app.pending_settings_confirm.is_none());
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
}

#[test]
fn settings_theme_persists_when_closed_with_back() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Graphics tab (index 3); row 0 = ThemePreset
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Graphics);

    app.update(Msg::Key(key(KeyCode::Down))); // row 1 = ThemePreset
    app.update(Msg::Key(key(KeyCode::Right))); // Default -> Midnight
    assert_eq!(app.theme.preset, "midnight");

    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.theme.preset, "midnight");

    let mut restored = App::new(100);
    restored.apply_config(saved);
    assert_eq!(restored.theme.preset, "midnight");
}

#[test]
fn settings_color_overrides_persist_when_quitting() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    let role = crate::theme::ThemeRole::Accent;
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = SettingsTab::Graphics;
        // ThemeColor rows start at field index 3 (after RetroMode, ThemePreset, BackgroundNone).
        st.row = 3 + crate::theme::ThemeRole::ALL
            .iter()
            .position(|&r| r == role)
            .unwrap();
        st.draft.theme.set_override(role, "#123456").unwrap();
        app.theme = st.draft.theme.normalized();
    }

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
    assert!(app.should_quit);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(
        saved.theme.overrides.get("accent").map(String::as_str),
        Some("#123456")
    );
}

#[test]
fn settings_close_applies_and_persists() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General)
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab; row 0 = Speed
    app.update(Msg::Key(key(KeyCode::Right))); // speed 1.0 -> 1.1 (draft)
    assert!(
        (app.playback.speed - 1.0).abs() < 1e-9,
        "committed speed unchanged while editing"
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    assert_eq!(app.mode, Mode::Player);
    assert!(
        (app.playback.speed - 1.1).abs() < 1e-9,
        "speed applied on close"
    );
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.speed, Some(1.1));
}

#[test]
fn settings_close_persists_live_audio() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback; Speed
    app.update(Msg::Key(key(KeyCode::Right))); // draft speed -> 1.1
    let cmds = app.update(Msg::Key(key(KeyCode::Esc))); // save+quit
    assert_eq!(app.mode, Mode::Player);
    assert!(
        (app.playback.speed - 1.1).abs() < 1e-9,
        "speed persisted on close"
    );
    assert_eq!(
        save_config(&cmds).expect("a SaveConfig cmd").speed,
        Some(1.1)
    );
    // Closing re-asserts the committed filter chain so the running track matches the
    // now-persisted settings.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Player(PlayerCmd::SetAudioFilter(_))))
    );
}

#[test]
fn settings_band_edit_sets_custom_and_emits_filter() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..5 {
        // Speed → Seek → Wheel volume → Gapless → EqPreset → Band(0) at row 5.
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Right))); // raise the band
    let st = app.settings.as_ref().unwrap();
    assert_eq!(st.draft.eq_preset, EqPreset::Custom);
    assert!(st.draft.eq_bands[0] > 0.0);
    // First non-zero band → full rebuild (creates the labels).
    assert!(cmds.iter().any(
        |c| matches!(c, Cmd::Player(PlayerCmd::SetAudioFilter(s)) if s.contains("equalizer"))
    ));
    // A second nudge with labels present uses the glitch-free af-command path.
    let cmds = app.update(Msg::Key(key(KeyCode::Right)));
    assert!(cmds.iter().any(|c| matches!(c,
        Cmd::Player(PlayerCmd::AfCommand { label, param, .. }) if label == "eq0" && param == "gain")));
}

#[test]
fn settings_close_reasserts_audio_and_persists_volume() {
    let mut app = app_playing(1, 0);
    app.playback.volume = 55; // a `=`/`-` change during the session
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..5 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 5
    }
    app.update(Msg::Key(key(KeyCode::Right))); // raise it (draft = Custom)
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    // Closing re-asserts the committed chain so the current track matches what was saved
    // even if an EOF rebuilt mpv from the old bands mid-edit.
    assert!(cmds.iter().any(|c| matches!(c,
        Cmd::Player(PlayerCmd::SetAudioFilter(s)) if s.contains("equalizer"))));
    // The session volume is folded into the persisted config (not the startup value).
    assert_eq!(save_config(&cmds).expect("a SaveConfig cmd").volume, 55);
}

#[test]
fn settings_preset_selector_snaps_from_custom_to_flat() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..5 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 5
    }
    app.update(Msg::Key(key(KeyCode::Right))); // hand-tune → Custom
    assert_eq!(
        app.settings.as_ref().unwrap().draft.eq_preset,
        EqPreset::Custom
    );
    app.update(Msg::Key(key(KeyCode::Up))); // back to the preset row
    // From Custom, the first ←/→ snaps to Flat rather than jumping to a neighbour.
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.eq_preset,
        EqPreset::Flat
    );
    // Then it cycles normally.
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.eq_preset,
        EqPreset::BassBoost
    );
}

#[test]
fn settings_text_field_edits_path_buffer() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General); row 0 = language
    let cookies_row = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::CookiesFile)
        .expect("cookies file field");
    for _ in 0..cookies_row {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Enter))); // enter text-edit mode
    assert!(app.settings.as_ref().unwrap().editing_text);
    for c in "/x.txt".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    // `q` is typed, not treated as close, while editing.
    assert_eq!(app.mode, Mode::Settings);
    app.update(Msg::Key(key(KeyCode::Enter))); // commit edit mode
    assert!(!app.settings.as_ref().unwrap().editing_text);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    assert_eq!(
        save_config(&cmds).unwrap().cookies_file,
        Some(std::path::PathBuf::from("/x.txt"))
    );
}

#[test]
fn settings_ai_tab_switches_model_live_and_persists() {
    let mut app = app_playing(1, 0);
    let start = app.ai.model;
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // row 0 = AiEnabled → row 1 = model
    app.update(Msg::Key(key(KeyCode::Right))); // cycle model (draft only)
    let drafted = app.settings.as_ref().unwrap().draft.gemini_model;
    assert_ne!(drafted, start, "← /→ cycles the model in the draft");
    assert_eq!(
        app.ai.model, start,
        "committed model unchanged while editing"
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    assert_eq!(app.ai.model, drafted, "model committed on close");
    // The running actor is told to hot-swap; config persists the choice.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SetAiModel(m) if *m == drafted))
    );
    assert_eq!(save_config(&cmds).unwrap().gemini_model, drafted);
}

#[test]
fn settings_ai_tab_edits_masked_api_key() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // start editing the key
    assert!(app.settings.as_ref().unwrap().editing_text);
    for c in "AIzaKey".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    // Committing the edit (Enter) persists the key immediately — it must NOT depend on
    // the user also pressing `s`, which is the trap that lost keys before.
    let cmds = app.update(Msg::Key(key(KeyCode::Enter))); // commit edit
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("AIzaKey")
    );
    // A new key rebuilds the assistant live (no relaunch), not just persists it.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ReloadAi { key: Some(k), .. } if k == "AIzaKey")),
        "committing a changed key must reload the DJ Gem actor"
    );
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::SetAiModel(_))));
    // The committed value is now in config, so a later close doesn't double-reload.
    let save_cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(
        save_config(&save_cmds).unwrap().gemini_api_key.as_deref(),
        Some("AIzaKey")
    );
    assert!(
        !save_cmds.iter().any(|c| matches!(c, Cmd::ReloadAi { .. })),
        "an unchanged key shouldn't rebuild the actor again on close"
    );
}

#[test]
fn api_key_persists_when_leaving_settings_via_close() {
    // The reported bug: type a key, then leave with Esc/q (the intuitive move) — the
    // key must survive.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // start editing
    for c in "AIzaPersist".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    // Esc commits the field (and persists it) rather than discarding the typed key.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("AIzaPersist")
    );
    // Esc again leaves the screen; config already holds the key.
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.gemini_api_key.as_deref(), Some("AIzaPersist"));
}

#[test]
fn opening_then_leaving_key_editor_empty_keeps_existing_key() {
    // Entering the masked editor clears the buffer; backing out without typing must
    // restore the saved key, not wipe it.
    let mut app = app_playing(1, 0);
    app.config.gemini_api_key = Some("KEEPME".to_owned());
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // start editing -> buffer cleared
    let cmds = app.update(Msg::Key(key(KeyCode::Esc))); // leave editor without typing
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("KEEPME"),
        "an untouched secret edit must not wipe the saved key"
    );
}

#[test]
fn editing_existing_api_key_starts_fresh_not_appended() {
    let mut app = app_playing(1, 0);
    app.config.gemini_api_key = Some("OLDKEY".to_owned());
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // start editing -> masked buffer cleared
    assert_eq!(
        app.settings.as_ref().unwrap().draft.gemini_api_key,
        "",
        "editing a secret field clears it rather than appending blindly"
    );
    for c in "NEWKEY".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter))); // commit
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    // Replaces, not "OLDKEYNEWKEY".
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("NEWKEY")
    );
}

#[test]
fn clicking_away_from_secret_editor_keeps_the_saved_key() {
    // Opening the masked editor clears the buffer and stashes the prior key. Moving focus via
    // the mouse path (settings_focus_row) must restore that stash — not leave an empty buffer
    // that erases the key on close. (Regression: the mouse focus-row used to skip the
    // edit-finish that restores the secret.)
    let mut app = app_playing(1, 0);
    app.config.gemini_api_key = Some("KEEPME".to_owned());
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // start editing -> buffer cleared, key stashed
    assert_eq!(app.settings.as_ref().unwrap().draft.gemini_api_key, "");

    // A click on another control re-focuses its row through this path.
    app.settings_focus_row(0);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.gemini_api_key,
        "KEEPME",
        "focusing away from an untouched secret edit restores the stashed key"
    );
    assert!(!app.settings.as_ref().unwrap().editing_text);

    // And it survives the save-on-close.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("KEEPME")
    );
}

#[test]
fn reset_all_re_enables_ai() {
    // Reset All must restore *every* field to its default, including the DJ Gem on/off switch —
    // otherwise a user who disabled DJ Gem then reset would be stranded with DJ Gem off.
    let mut app = app_playing(1, 0);
    app.config.ai_enabled = Some(false);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft.ai_enabled seeds false)
    assert!(!app.settings.as_ref().unwrap().draft.ai_enabled);
    app.settings_reset_all();
    assert!(
        app.settings.as_ref().unwrap().draft.ai_enabled,
        "reset returns DJ Gem to its default (enabled)"
    );
}

// --- G: DJ Gem assistant ----------------------------------------------------

/// The prompt of the `AskAi` command among `cmds`, if any.
fn ask_ai(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::AskAi { prompt, .. } => Some(prompt.as_str()),
        _ => None,
    })
}

fn radio_fallback(cmds: &[Cmd]) -> Option<(&str, &str, &[String])> {
    cmds.iter().find_map(|c| match c {
        Cmd::RadioFallback {
            seed,
            seed_video_id,
            exclude_ids,
            ..
        } => Some((
            seed.as_str(),
            seed_video_id.as_str(),
            exclude_ids.as_slice(),
        )),
        _ => None,
    })
}

/// The `(seed_video_id, prompt)` of the `AiRerank` command among `cmds`, if any.
fn ai_rerank(cmds: &[Cmd]) -> Option<(&str, &str)> {
    cmds.iter().find_map(|c| match c {
        Cmd::AiRerank {
            seed_video_id,
            prompt,
        } => Some((seed_video_id.as_str(), prompt.as_str())),
        _ => None,
    })
}

#[test]
fn g_enters_ai_from_player_and_library() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai);
    assert_eq!(app.ai.focus, AiFocus::Input);
    // And from the library view.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai);
}

#[test]
fn ai_submit_without_key_shows_onboarding_error() {
    let mut app = app_playing(1, 0); // ai_available defaults to false
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "play jazz".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(ask_ai(&cmds).is_none(), "no AskAi without a key");
    assert!(!app.ai.thinking);
    // Transcript holds the user prompt plus an error line.
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
    assert!(
        app.ai
            .messages
            .iter()
            .any(|m| m.role == AiRole::User && m.text == "play jazz")
    );
}

#[test]
fn ai_submit_with_key_emits_ask_and_sets_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "play lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(ask_ai(&cmds), Some("play lofi"));
    assert!(app.ai.thinking);
    assert!(app.ai.input.is_empty());
    // A second submit while thinking is ignored (no duplicate request).
    for c in "more".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(ask_ai(&cmds).is_none());
}

#[test]
fn ai_play_tracks_on_empty_queue_starts_playback() {
    let mut app = App::new(100);
    assert!(app.queue.is_empty());
    let cmds = app.update(Msg::AiPlayTracks(songs(3)));
    assert_eq!(current(&app), "id0");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id0"));
}

#[test]
fn ai_enqueue_reports_count_and_extends() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue has id0, id1
    app.update(Msg::AiEnqueue(songs(3)));
    assert_eq!(app.queue.len(), 5);
    assert!(app.status.text.contains("Queued"));
}

#[test]
fn ai_error_clears_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.thinking = true;
    app.update(Msg::AiError("boom".to_owned()));
    assert!(!app.ai.thinking);
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
}

#[test]
fn ai_empty_chat_is_not_appended() {
    let mut app = app_playing(1, 0);
    app.update(Msg::AiChat("   ".to_owned()));
    assert!(app.ai.messages.is_empty());
    app.update(Msg::AiChat("here you go".to_owned()));
    assert_eq!(app.ai.messages.len(), 1);
}

#[test]
fn ai_radio_circuit_breaker_disables_after_repeated_empties() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_radio = true;
    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.update(Msg::AiEnqueue(Vec::new())); // resolves nothing
    }
    assert!(
        !app.autoplay_radio,
        "radio disabled after repeated empty extends"
    );
    assert!(app.status.text.contains("Autoplay radio stopped"));
}

#[test]
fn autoplay_extends_when_queue_runs_low() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.ai.available = true;
    app.autoplay_radio = true;
    // A manual next advances and should fetch the candidate pool first (both DJ Gem and non-DJ Gem
    // paths share one pool; the DJ Gem reranks it once it returns).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(
        radio_fallback(&cmds).is_some(),
        "autoplay should fetch a candidate pool"
    );
    assert!(
        ask_ai(&cmds).is_none(),
        "no free-form DJ Gem radio prompt anymore"
    );
    assert!(app.radio.pending);
    assert!(
        !app.ai.thinking,
        "the rerank only starts once the pool returns"
    );
    // The cooldown / in-flight guard blocks an immediate second request.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(radio_fallback(&cmds).is_none());
}

#[test]
fn radio_tab_entries_do_not_feed_station_state() {
    let mut app = app_playing(1, 0);
    let normal_favorite = Song::remote("fav-song", "Favorite", "Song Artist", "0:10");
    let normal_history = Song::remote("hist-song", "History", "History Artist", "0:10");
    let radio_favorite = radio_station("fav-radio");
    let radio_recent = radio_station("recent-radio");

    app.library.favorites.push(normal_favorite);
    app.library.history.push_front(normal_history);
    app.library.radio_favorites.push(radio_favorite.clone());
    app.library.radios.push_front(radio_recent.clone());

    let st = app.build_station_state("id0");
    let normal_fav_artist = crate::signals::normalize_artist("Song Artist");
    let radio_artist = crate::signals::normalize_artist("KR / MP3");

    assert!(st.favorite_artist_keys.contains(&normal_fav_artist));
    assert!(!st.favorite_artist_keys.contains(&radio_artist));
    assert!(!st.recent_track_ids.contains(&radio_favorite.video_id));
    assert!(!st.recent_track_ids.contains(&radio_recent.video_id));
    assert!(!st.recent_artist_keys.contains(&radio_artist));
}

#[test]
fn ai_radio_hands_a_local_shortlist_to_the_reranker() {
    let mut app = app_playing(1, 0); // current id0 is already in history
    let current = app.queue.current().cloned().unwrap();
    app.library
        .record_play(&Song::remote("prev2", "previous two", "artist b", "0:10"));
    app.library
        .record_play(&Song::remote("prev1", "previous one", "artist a", "0:10"));
    app.library.record_play(&current); // current can be present in history; don't duplicate it.
    app.ai.available = true;
    app.autoplay_radio = true;

    // The fetched pool flows through the local engine; a diverse shortlist goes to the DJ Gem.
    let cmds = app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (
                Song::remote("cand1", "Track One", "band one", "3:00"),
                CandidateSource::WatchPlaylist,
            ),
            (
                Song::remote("cand2", "Track Two", "band two", "3:10"),
                CandidateSource::YtdlpRadio,
            ),
            (
                Song::remote("cand3", "Track Three", "band three", "3:20"),
                CandidateSource::WatchPlaylist,
            ),
        ],
    });

    let (seed_id, prompt) = ai_rerank(&cmds).expect("a DJ Gem rerank command");
    assert_eq!(seed_id, "id0");
    // Compact protocol header + candidate pack.
    assert!(prompt.contains("TASK|radio_next"));
    assert!(prompt.contains("CANDS"));
    // Recent session context (current + the two previous tracks).
    assert!(prompt.contains("- Current: t0 — a"));
    assert!(prompt.contains("- Previous 1: previous one — artist a"));
    assert!(prompt.contains("- Previous 2: previous two — artist b"));
    // Candidates appear by title under opaque cids; the raw video ids stay hidden so the
    // model can't read rank off them.
    assert!(prompt.contains("Track One"));
    assert!(prompt.contains("Track Two"));
    assert!(
        !prompt.contains("cand1"),
        "raw video ids must not leak into the pack"
    );
    assert!(app.ai.thinking, "the rerank is in flight");
    assert!(
        app.radio.pending_rerank.is_some(),
        "shortlist + local pick stashed for validation"
    );
    assert!(!app.radio.pending, "the pool fetch is done");
}

#[test]
fn smart_gate_skips_the_ai_call_and_enqueues_the_local_pick() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0, remaining 0 → a refill is due
    app.ai.available = true;
    app.autoplay_radio = true;
    // smart_gate is on by default; a negative ambiguity gap forces the score-gap branch to read
    // as confident, so this test isolates the gated local path.
    app.config.radio.ai.smart_gate = true;
    app.config.radio.ai.ambiguity_gap = -1.0;

    let before = app.queue.len();
    let src = CandidateSource::YtdlpRadio;
    let cmds = app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("cand1", "Track One", "band one", "3:00"), src),
            (Song::remote("cand2", "Track Two", "band two", "3:10"), src),
            (
                Song::remote("cand3", "Track Three", "band three", "3:20"),
                src,
            ),
        ],
    });

    assert!(
        ai_rerank(&cmds).is_none(),
        "gated: no DJ Gem rerank command spent"
    );
    assert!(
        !app.ai.thinking,
        "gated path never marks the assistant as thinking"
    );
    assert!(
        app.radio.pending_rerank.is_none(),
        "gated path stashes nothing to validate"
    );
    assert!(
        app.queue.len() > before,
        "gated refill enqueues the local pick directly"
    );
}

#[test]
fn ai_result_cache_replays_an_identical_refill_without_a_second_call() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0
    app.ai.available = true;
    app.autoplay_radio = true;
    // Force the call through the gate so this exercises the cache, not the smart gate.
    app.config.radio.ai.ambiguity_gap = 1.0;

    let src = CandidateSource::YtdlpRadio;
    let candidates = vec![
        (Song::remote("cand1", "Track One", "band one", "3:00"), src),
        (Song::remote("cand2", "Track Two", "band two", "3:10"), src),
        (
            Song::remote("cand3", "Track Three", "band three", "3:20"),
            src,
        ),
    ];

    // First refill misses the cache → a DJ Gem call goes out, the rerank is stashed, and (on the DJ Gem
    // path) the queue is left untouched, so the next refill recomputes the *same* cache key.
    let cmds = app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates: candidates.clone(),
    });
    assert!(ai_rerank(&cmds).is_some(), "first refill spends a call");
    let pending = app.radio.pending_rerank.as_ref().expect("rerank stashed");
    let key = pending.cache_key;
    let cached_id = pending.cid_map[0].video_id.clone(); // a real shortlist track

    // Seed the cache as if that rerank had resolved to `cached_id`, then clear the in-flight flags
    // (queue/history untouched → the next identical refill keys to the same entry).
    app.ai_cache_store(key, vec![cached_id.clone()]);
    app.radio.pending_rerank = None;
    app.ai.thinking = false;
    app.radio.pending = false;
    app.radio.last_extend = None;

    // Second identical refill hits the cache → no call; the cached ordering is enqueued directly.
    let cmds = app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates,
    });
    assert!(
        ai_rerank(&cmds).is_none(),
        "cache hit: no second DJ Gem call"
    );
    assert!(
        !app.ai.thinking,
        "cache hit never marks the assistant as thinking"
    );
    assert!(
        app.radio.pending_rerank.is_none(),
        "cache hit stashes nothing to validate"
    );
    assert!(
        app.queue.contains_video_id(&cached_id),
        "cached ordering enqueued"
    );
}

#[test]
fn ai_set_station_profile_applies_mode_and_avoids_artists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0

    let cmds = app.update(Msg::AiSetStationProfile {
        query: "rainy day".to_owned(),
        explore: Some("tight".to_owned()),
        avoid_artists: vec!["Nickelback".to_owned()],
    });

    // The explore level drives the live engine mode, and the profile is stashed for persistence.
    assert_eq!(app.config.radio.mode, crate::radio::RadioMode::Focused);
    assert_eq!(
        app.station.active.as_ref().expect("station stashed").query,
        "rainy day"
    );
    // Both the station and the (now-mode-changed) config are persisted.
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveStationProfile)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))));

    // The avoided artist flows into the station state every refill reads.
    let st = app.build_station_state("id0");
    let want = crate::signals::normalize_artist("Nickelback");
    assert!(
        st.banned_artist_keys.contains(&want),
        "avoided artist is banned in refills"
    );
}

#[test]
fn a_plain_start_radio_without_hints_leaves_no_station() {
    // The reducer only stamps a profile when the tool passes shaping hints; this asserts the
    // engine default holds when none are given (the tool simply omits the AiSetStationProfile msg).
    let app = app_playing(1, 0);
    assert!(app.station.active.is_none());
    assert!(app.build_station_state("id0").banned_artist_keys.is_empty());
}

#[test]
fn station_patch_folds_feedback_into_avoid_list_and_clears_inflight() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.station.active = Some(crate::station::StationProfile::from_intent(
        "late night",
        Some("wide"),
        &[],
    ));
    app.radio.feedback_in_flight = true;

    let cmds = app.update(Msg::StationPatch {
        down_artists: vec!["Nickelback".to_owned()],
        boost_artists: vec![],
    });

    // The in-flight guard always clears so the next streak can fire again.
    assert!(
        !app.radio.feedback_in_flight,
        "in-flight guard cleared on patch"
    );
    // The down-voted artist is now avoided in every refill, and the change is persisted.
    let want = crate::signals::normalize_artist("Nickelback");
    assert!(
        app.station
            .active
            .as_ref()
            .unwrap()
            .avoid_artist_keys
            .contains(&want)
    );
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveStationProfile)));
}

#[test]
fn empty_station_patch_clears_inflight_without_persisting() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.station.active = Some(crate::station::StationProfile::from_intent("q", None, &[]));
    app.radio.feedback_in_flight = true;

    // An empty patch (the off-path summary failed or found nothing) still clears the guard, but a
    // no-op change must not trigger a pointless save.
    let cmds = app.update(Msg::StationPatch {
        down_artists: vec![],
        boost_artists: vec![],
    });
    assert!(!app.radio.feedback_in_flight);
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::SaveStationProfile)),
        "no save on a no-op patch"
    );
}

#[test]
fn feedback_summary_fires_once_per_skip_streak_when_gated_open() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    app.station.active = Some(crate::station::StationProfile::from_intent(
        "drive",
        Some("balanced"),
        &[],
    ));
    // A trailing skip streak at the trigger threshold (FEEDBACK_STREAK = 3).
    for _ in 0..3 {
        app.record_session_event("some artist", Outcome::QuickSkip, 0.05);
    }

    // First call past the gate: dispatches a summary and arms the in-flight guard.
    let cmd = app.maybe_summarize_feedback();
    assert!(
        matches!(cmd, Some(Cmd::SummarizeFeedback { .. })),
        "streak + active station → summary"
    );
    assert!(app.radio.feedback_in_flight);
    // A second call while one is in flight is a no-op (single-flight).
    assert!(
        app.maybe_summarize_feedback().is_none(),
        "in-flight guard suppresses duplicates"
    );
}

#[test]
fn feedback_summary_is_skipped_without_an_active_station() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    for _ in 0..3 {
        app.record_session_event("x", Outcome::Skip, 0.1);
    }
    // No station to refine → nothing to learn, so no call (and no guard armed).
    assert!(app.maybe_summarize_feedback().is_none());
    assert!(!app.radio.feedback_in_flight);
}

#[test]
fn radio_ai_picks_enqueue_validated_ids_and_top_up_from_local() {
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_radio = true;
    app.ai.thinking = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::radio::RadioMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "S1", "a", "3:00"),
            Song::remote("s2", "S2", "b", "3:00"),
        ],
        local_pick: vec![
            Song::remote("s2", "S2", "b", "3:00"),
            Song::remote("s1", "S1", "a", "3:00"),
        ],
        cid_map: vec![
            crate::radio::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::radio::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    // DJ Gem picks one valid cid + one hallucinated cid (dropped); the gap tops up from local.
    app.update(Msg::RadioAiPicks {
        seed_video_id: "id0".to_owned(),
        picks: vec![
            AiPick {
                cid: "c1".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec!["u".to_owned()],
            },
            AiPick {
                cid: "HALLUCINATED".to_owned(),
                role: None,
                reasons: vec![],
            },
        ],
        conf: Some(0.8),
    });

    assert!(!app.ai.thinking, "rerank finished");
    assert!(app.radio.pending_rerank.is_none(), "pending consumed");
    assert!(
        app.queue.contains_video_id("s1"),
        "valid DJ Gem id enqueued"
    );
    assert!(
        app.queue.contains_video_id("s2"),
        "topped up from local pick"
    );
    assert!(
        !app.queue.contains_video_id("HALLUCINATED"),
        "hallucinated id dropped"
    );
}

#[test]
fn radio_ai_picks_for_a_stale_seed_are_ignored() {
    let mut app = app_playing(2, 0);
    app.ai.available = true;
    app.autoplay_radio = true;
    app.ai.thinking = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "current-seed".to_owned(),
        mode: crate::radio::RadioMode::Balanced,
        shortlist: vec![Song::remote("s1", "S1", "a", "3:00")],
        local_pick: vec![Song::remote("s1", "S1", "a", "3:00")],
        cid_map: vec![crate::radio::PackedCand {
            cid: "c1".to_owned(),
            video_id: "s1".to_owned(),
        }],
        cache_key: 0,
    });

    // A result for a different (older) seed must not consume the in-flight rerank.
    app.update(Msg::RadioAiPicks {
        seed_video_id: "old-seed".to_owned(),
        picks: vec![AiPick {
            cid: "c1".to_owned(),
            role: None,
            reasons: vec![],
        }],
        conf: None,
    });
    assert!(
        app.radio.pending_rerank.is_some(),
        "stale result leaves the current rerank intact"
    );
    assert!(!app.queue.contains_video_id("s1"));
}

#[test]
fn why_ai_overlay_explains_the_last_ai_rerank() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_radio = true;
    app.ai.thinking = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::radio::RadioMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "First Song", "Artist One", "3:00"),
            Song::remote("s2", "Second Song", "Artist Two", "3:00"),
        ],
        local_pick: vec![Song::remote("s2", "Second Song", "Artist Two", "3:00")],
        cid_map: vec![
            crate::radio::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::radio::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    app.update(Msg::RadioAiPicks {
        seed_video_id: "id0".to_owned(),
        picks: vec![
            AiPick {
                cid: "c1".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned(), "u".to_owned()],
            },
            AiPick {
                cid: "c2".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec!["co".to_owned()],
            },
        ],
        conf: Some(0.75),
    });

    // The explanation is stashed, with cids resolved to real tracks in the model's order.
    let explain = app
        .radio
        .last_explain
        .as_ref()
        .expect("explanation stashed for the overlay");
    assert_eq!(explain.conf, Some(0.75));
    assert_eq!(explain.picks.len(), 2);
    assert_eq!(explain.picks[0].title, "First Song");
    assert_eq!(explain.picks[0].artist, "Artist One");
    assert_eq!(explain.picks[0].role.as_deref(), Some("bridge"));
    assert_eq!(explain.picks[0].reasons, vec!["tr", "u"]);
    assert_eq!(explain.picks[1].title, "Second Song");

    // `w` opens the overlay; `w` again dismisses it.
    assert!(!app.why_ai_visible);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert!(app.radio_dedicated_mode);
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        app.why_ai_visible,
        "w opens the Why-DJ Gem overlay in Radio mode"
    );
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(!app.why_ai_visible, "w again dismisses it");
}

#[test]
fn why_ai_without_a_rerank_shows_a_note_not_an_overlay() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.status.text.clear();
    assert!(app.radio.last_explain.is_none());

    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        !app.why_ai_visible,
        "no overlay opens without a prior DJ Gem rerank"
    );
    assert!(
        !app.status.text.is_empty(),
        "a transient note is shown instead"
    );
}

#[test]
fn why_ai_overlay_renders_the_resolved_picks() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.radio.last_explain = Some(RadioAiExplain {
        conf: Some(0.82),
        picks: vec![
            ExplainPick {
                title: "Bridge Track".to_owned(),
                artist: "Some Artist".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned(), "u".to_owned()],
            },
            ExplainPick {
                title: "Core Track".to_owned(),
                artist: "Another Artist".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec![],
            },
        ],
    });
    app.why_ai_visible = true;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap(); // must not panic
    let buf = terminal.backend().buffer().clone();
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        text.contains("Bridge Track"),
        "overlay shows the first resolved track"
    );
    assert!(
        text.contains("Core Track"),
        "overlay shows the second resolved track"
    );
}

#[test]
fn autoplay_uses_radio_fallback_without_ai_key() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.autoplay_radio = true;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(
        ask_ai(&cmds).is_none(),
        "no Gemini request without an API key"
    );
    let (seed, seed_video_id, exclude_ids) =
        radio_fallback(&cmds).expect("a fallback radio command");
    assert_eq!(seed_video_id, "id1");
    assert!(seed.contains("t1"));
    assert!(exclude_ids.iter().any(|id| id == "id0"));
    assert!(exclude_ids.iter().any(|id| id == "id1"));
    assert!(app.radio.pending);

    let cmds = app.maybe_autoplay_extend();
    assert!(
        radio_fallback(&cmds).is_none(),
        "pending fallback blocks duplicate requests"
    );
}

#[test]
fn radio_results_run_through_local_engine_and_clear_pending() {
    let _guard = crate::i18n::lock_for_test();
    fastrand::seed(7);
    let mut app = app_playing(2, 0);
    app.autoplay_radio = true;
    app.radio.pending = true;

    // The local engine excludes the seed (id0) and the already-queued track (id1), dedups
    // the repeated id2, and ranks the rest. Distinct artists + normal durations keep the
    // two survivors out of the artist-cooldown / duration hard filters, so both enqueue.
    let src = CandidateSource::YtdlpRadio;
    app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("id0", "current", "a", "3:00"), src), // == seed, dropped
            (Song::remote("id2", "New Song", "c", "3:00"), src), // kept
            (Song::remote("id2", "New Song", "c", "3:00"), src), // canonical duplicate, deduped
            (Song::remote("id1", "queued", "b", "3:00"), src),  // already queued, dropped
            (Song::remote("id3", "Another", "d", "3:00"), src), // kept
        ],
    });

    assert!(!app.radio.pending, "results clear the in-flight guard");
    assert_eq!(
        app.queue.len(),
        4,
        "two new tracks added to the queue of two"
    );
    assert!(app.queue.contains_video_id("id2"));
    assert!(app.queue.contains_video_id("id3"));
    assert!(app.status.text.contains("Queued 2"));
}

#[test]
fn radio_error_uses_circuit_breaker() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_radio = true;

    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.radio.pending = true;
        app.update(Msg::RadioError {
            seed_video_id: "id0".to_owned(),
            error: "yt-dlp failed".to_owned(),
        });
    }

    assert!(!app.radio.pending);
    assert!(!app.autoplay_radio);
    assert!(app.status.text.contains("Autoplay radio stopped"));
}

#[test]
fn ai_create_and_play_playlist_roundtrip() {
    let mut app = App::new(100);
    let cmds = app.update(Msg::AiCreatePlaylist("Focus".to_owned()));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SavePlaylists)));
    app.update(Msg::AiAddToPlaylist {
        playlist: "Focus".to_owned(),
        songs: songs(2),
    });
    assert_eq!(app.playlists.find("Focus").unwrap().songs.len(), 2);
    let cmds = app.update(Msg::AiPlayPlaylist("Focus".to_owned()));
    assert_eq!(current(&app), "id0");
    assert!(load_url(&cmds).is_some());
}

// --- M5: library (favorites + history) ----------------------------------

#[test]
fn f_toggles_favorite_of_current_track() {
    let mut app = app_playing(3, 0); // playing "id0"
    assert!(!app.library.is_favorite("id0"));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite("id0"));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    app.update(Msg::Key(key(KeyCode::Char('f')))); // toggle off
    assert!(!app.library.is_favorite("id0"));
}

#[test]
fn playing_records_history_most_recent_first() {
    let mut app = app_playing(3, 0); // loads id0 -> history [id0]
    app.update(Msg::Key(key(KeyCode::Char('n')))); // id1 -> [id1, id0]
    let hist: Vec<&str> = app
        .library
        .history
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(hist, vec!["id1", "id0"]);
}

#[test]
fn playing_radio_records_radio_tab_only() {
    let mut app = App::new(100);
    let station = radio_station("station-a");
    app.queue.set(vec![station.clone()], 0);
    let cmds = app.load_song(app.queue.current().cloned());

    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(app.library.history.is_empty());
    assert!(app.library.favorites.is_empty());
    assert_eq!(
        app.library.radios.front().map(|s| s.video_id.as_str()),
        Some("rad:station-a")
    );

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::All;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::History;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Favorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Radio;
    assert_eq!(row_ids(&app), vec!["rad:station-a"]);
}

#[test]
fn radio_favorite_is_separate_from_song_favorites() {
    let mut app = App::new(100);
    let station = radio_station("station-fav");
    app.search.results = vec![station.clone()];
    app.search.focus = SearchFocus::Results;
    app.mode = Mode::Search;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));

    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(app.library.favorites.is_empty());
    assert!(app.library.history.is_empty());
    assert_eq!(app.library.radio_favorites.len(), 1);
    assert_eq!(app.library.radio_favorites[0].video_id, "rad:station-fav");
    assert!(app.library.is_favorite("rad:station-fav"));

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::All;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert_eq!(row_ids(&app), vec!["rad:station-fav"]);
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());
}

#[test]
fn favorite_from_search_results() {
    let mut app = App::new(100);
    app.search.results = songs(3);
    app.search.selected = 1;
    app.search.focus = SearchFocus::Results;
    app.mode = Mode::Search;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite("id1"));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
}

#[test]
fn l_opens_library_and_enter_plays_selected() {
    let mut app = app_playing(3, 0);
    // favorites become [id0, id1] (most-recent-first insertion).
    app.library.toggle_favorite(&songs(2)[1]);
    app.library.toggle_favorite(&songs(2)[0]);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Down))); // select all[1] = id1
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "id1");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
}

#[test]
fn other_screens_keep_remapped_confirm_key() {
    // Library isn't hardwired to Enter the way Search is: a remapped Common Confirm key (F5)
    // still plays there, via the Common fallback. (Library also keeps its own Enter→play
    // binding, like the Queue window — see `enter_on_library_plays_selected_song...`.)
    let mut app = app_playing(3, 0);
    app.keymap = confirm_on_f5_keymap();
    app.library.toggle_favorite(&songs(2)[1]);
    app.library.toggle_favorite(&songs(2)[0]);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(key(KeyCode::Down))); // select all[1] = id1

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "id1");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
}

#[test]
fn q_closes_library_without_quitting_app() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn library_tab_toggles_and_unfavorite_fixes_selection() {
    let mut app = app_playing(1, 0);
    app.library.toggle_favorite(&songs(1)[0]); // favorites = [id0]
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::History);
    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    // Unfavorite the only entry: selection clamps to 0, list empties.
    app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert_eq!(app.library_ui.selected, 0);
    assert!(app.library.favorites.is_empty());
}

#[test]
fn library_all_includes_downloaded_tracks_and_loads_local_path() {
    let mut app = App::new(100);
    let local = Song::local_file(PathBuf::from("/tmp/local-track.m4a"));
    app.library_ui.downloaded = vec![local.clone()];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    assert_eq!(app.library_len(), 1);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(load_url(&cmds), Some("/tmp/local-track.m4a"));
    assert_eq!(app.queue.current().unwrap().video_id, local.video_id);
}

#[test]
fn downloads_tab_shows_download_folder_tracks() {
    let mut app = App::new(100);
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/a.m4a"))];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::BackTab))); // All -> Downloads
    assert_eq!(app.library_ui.tab, LibraryTab::Downloads);
    assert_eq!(app.library_len(), 1);
}

// --- in-library filter (`/`) -------------------------------------------------

fn fsong(id: &str, title: &str, artist: &str) -> Song {
    Song::remote(id, title, artist, "0:10")
}

/// Opens the Favorites tab with the given favorites (set directly for a deterministic order).
fn app_with_favorites(favs: Vec<Song>) -> App {
    let mut app = App::new(100);
    app.library.favorites = favs;
    app.update(Msg::Key(key(KeyCode::Char('l')))); // open library (All)
    app.update(Msg::Key(key(KeyCode::Tab))); // All -> Favorites
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app
}

fn row_ids(app: &App) -> Vec<String> {
    app.library_rows()
        .iter()
        .map(|s| s.video_id.clone())
        .collect()
}

#[test]
fn slash_is_bound_to_library_filter() {
    let app = App::new(100);
    let slash = Chord::new(KeyCode::Char('/'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Library, slash),
        Some(Action::LibraryFilter)
    );
}

#[test]
fn slash_opens_filter_and_typing_narrows_the_list() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    assert_eq!(app.library_len(), 3);

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.library_ui.filter_editing);
    for c in "lov".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_ui.filter_query, "lov");
    assert_eq!(row_ids(&app), vec!["a"]);
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.library_ui.selected, 0);
}

#[test]
fn filter_matches_title_or_artist_case_insensitively() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "BILLIE".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    // Matched by artist, ignoring case.
    assert_eq!(row_ids(&app), vec!["a", "b"]);
}

#[test]
fn empty_filter_query_is_passthrough() {
    let mut app = app_with_favorites(vec![fsong("a", "One", "x"), fsong("b", "Two", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/')))); // open, but type nothing
    assert!(app.library_ui.filter_editing);
    assert_eq!(app.library_len(), 2); // full list while query is empty
}

#[test]
fn enter_commits_filter_and_esc_clears_it() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Char('o'))));

    // Enter commits: stop editing, keep the filter applied.
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.library_ui.filter_editing);
    assert_eq!(app.library_ui.filter_query, "lo");
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.mode, Mode::Library);

    // Esc with a committed filter clears it (full list back) and stays in the Library.
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.filter_query.is_empty());
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.library_len(), 2);
}

#[test]
fn esc_while_editing_cancels_the_filter() {
    let mut app = app_with_favorites(vec![fsong("a", "One", "x"), fsong("b", "Two", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.update(Msg::Key(key(KeyCode::Char('n')))); // "on" matches "One" only
    assert_eq!(app.library_len(), 1);
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.library_ui.filter_editing);
    assert!(app.library_ui.filter_query.is_empty());
    assert_eq!(app.library_len(), 2);
}

#[test]
fn delete_under_filter_removes_the_matched_track_by_identity() {
    // Raw order [k0, m, k1]: a naive index-based delete of filtered-row-0 would wrongly
    // remove k0; the identity-based delete must remove only the matched track m.
    let mut app = app_with_favorites(vec![
        fsong("k0", "Alpha", "x"),
        fsong("m", "Zebra", "x"),
        fsong("k1", "Beta", "x"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "zeb".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_len(), 1);
    app.update(Msg::Key(key(KeyCode::Enter))); // commit; selection on filtered row 0

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    let ids: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["k0", "k1"]);
}

#[test]
fn filter_clears_when_switching_tabs() {
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit so Tab is dispatched as a tab switch
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Tab))); // Favorites -> History, drops the filter
    assert_eq!(app.library_ui.tab, LibraryTab::History);
    assert!(app.library_ui.filter_query.is_empty());
    assert!(!app.library_ui.filter_editing);
}

#[test]
fn delete_after_navigating_then_filtering_removes_only_the_cursor_row() {
    // Regression: moving the cursor down plants the multi-select anchor at that row. Opening
    // the filter and typing must snap the anchor back to 0, or a single Delete on the
    // committed filter would silently wipe the whole anchor..=cursor range of matches.
    let mut app = app_with_favorites(vec![
        fsong("a", "Alpha", "x"),
        fsong("b", "Beta", "x"),
        fsong("c", "Carol", "x"),
        fsong("d", "Delta", "x"),
    ]);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down))); // cursor + anchor now at raw row 2
    assert_eq!(app.library_ui.anchor, 2);

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('a')))); // matches all four (every title has 'a')
    assert_eq!(app.library_len(), 4);
    assert_eq!(app.library_ui.selected, 0);
    assert_eq!(app.library_ui.anchor, 0); // the fix: anchor followed the cursor to 0
    app.update(Msg::Key(key(KeyCode::Enter))); // commit

    app.update(Msg::Key(key(KeyCode::Delete)));
    let ids: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["b", "c", "d"]); // only the cursor row (Alpha) went, not a range
}

#[test]
fn playing_the_whole_tab_under_a_filter_queues_only_the_filtered_subset() {
    // WYSIWYG: with a filter applied, "play whole tab" (P) plays — and queues — only the
    // visible matches, consistent with delete/favorite/download all operating on the filtered set.
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "billie".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter))); // commit; two matches
    assert_eq!(app.library_len(), 2);

    app.update(Msg::Key(key(KeyCode::Char('a')))); // play the whole filtered tab
    assert_eq!(app.mode, Mode::Player);
    let ids: Vec<&str> = app.queue.video_ids().collect();
    assert_eq!(ids, vec!["a", "b"]); // only the matched tracks were queued
}

#[test]
fn q_closes_the_library_in_one_press_even_with_a_filter_applied() {
    // `q` is the one-press "close Library"; clearing the filter is Esc's lighter gesture.
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit the filter
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player); // closed on the first press
}

#[test]
fn reopening_the_library_clears_a_leftover_filter() {
    // Leaving with a filter applied is harmless: the next Library open starts clean.
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit
    app.update(Msg::Key(key(KeyCode::Char('q')))); // leave; filter state lingers in Player
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Char('l')))); // re-open the Library
    assert_eq!(app.mode, Mode::Library);
    assert!(app.library_ui.filter_query.is_empty()); // OpenLibrary reset it
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites); // tab persists, only the filter resets
    assert_eq!(app.library_len(), 2);
}

// --- copy link (`y`): only internet-sourced tracks expose a URL --------------

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
fn bare_local(path: &str, title: &str) -> Song {
    Song {
        video_id: format!("local:{path}"),
        title: title.to_owned(),
        artist: "Local file".to_owned(),
        duration: String::new(),
        source: SearchSource::Youtube,
        playable: None,
        local_path: Some(PathBuf::from(path)),
        yt_video_id: None,
    }
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
    assert!(load_url(&cmds).is_some());
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
        cmds.iter().any(|c| matches!(c, Cmd::SaveDownloads)),
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
fn download_done_with_empty_path_does_not_save() {
    let mut app = App::new(100);
    let cmds = app.update(Msg::DownloadDone {
        video_id: "x".to_owned(),
        path: "   ".to_owned(),
    });
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::SaveDownloads)));
}

// --- library multi-select delete (drag + Delete), per-tab semantics ------

/// A real, empty audio file in the temp dir, named uniquely so parallel tests don't clash.
fn temp_audio_file(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "ytm-tui-app-test-{}-{tag}-{nanos}.m4a",
        std::process::id()
    ));
    std::fs::write(&path, b"").unwrap();
    path
}

/// Open the library and switch to `tab` by tab-key presses (All is the entry tab).
fn open_library_tab(app: &mut App, tab: LibraryTab) {
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    while app.library_ui.tab != tab {
        app.update(Msg::Key(key(KeyCode::Tab)));
    }
}

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
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
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
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
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

#[test]
fn library_mouse_drag_selects_range_then_delete_removes_it() {
    let mut app = App::new(100);
    app.library
        .toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("b", "tb", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("c", "tc", "x", "0:10")); // [c, b, a]
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;

    // Render so the per-row hit rects are published.
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row_rect = |i: usize| {
        app.bridges
            .mouse_buttons
            .borrow()
            .iter()
            .find(|b| b.target == MouseTarget::ListRow(i))
            .map(|b| b.rect)
            .expect("a rendered library row rect")
    };
    let r0 = row_rect(0);
    let r2 = row_rect(2);

    // Click row 0 (anchors the range), then drag onto row 2 (extends it).
    app.update(Msg::MouseClick {
        col: r0.x,
        row: r0.y,
    });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (0, 0));
    app.update(Msg::MouseDrag {
        col: r2.x,
        row: r2.y,
    });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (2, 0));

    // Delete removes the whole selected 0..=2 range.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library.favorites.is_empty());
}

#[test]
fn library_drag_after_release_starts_a_fresh_range() {
    let mut app = App::new(100);
    app.library.favorites = vec![
        Song::remote("a", "ta", "x", "0:10"),
        Song::remote("b", "tb", "x", "0:10"),
        Song::remote("c", "tc", "x", "0:10"),
        Song::remote("d", "td", "x", "0:10"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    render_app(&app);

    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));
    let (c3, r3) = button_center(&app, MouseTarget::ListRow(3));

    app.update(Msg::MouseClick { col: c0, row: r0 });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (0, 0));
    app.update(Msg::MouseLeftUp);

    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!(
        (app.library_ui.selected, app.library_ui.anchor),
        (2, 2),
        "a new drag after release must not extend the old row-0 selection"
    );
    app.update(Msg::MouseDrag { col: c3, row: r3 });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (3, 2));
}

// --- M6: lyrics ---------------------------------------------------------

fn lyric_lines() -> Vec<LyricLine> {
    vec![
        LyricLine {
            time: 0.0,
            text: "one".to_owned(),
        },
        LyricLine {
            time: 5.0,
            text: "two".to_owned(),
        },
    ]
}

#[test]
fn shift_l_toggles_lyrics_and_fetches_on_open() {
    let mut app = app_playing(3, 0); // playing id0
    let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
    assert!(app.lyrics.visible);
    assert!(app.lyrics.loading);
    match cmds.as_slice() {
        [Cmd::FetchLyrics { video_id, .. }] => assert_eq!(video_id, "id0"),
        _ => panic!("expected a FetchLyrics cmd"),
    }
    // Toggling off issues no fetch.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
    assert!(!app.lyrics.visible);
    assert!(cmds.is_empty());
}

#[test]
fn lyrics_result_stored_only_for_current_track() {
    let mut app = app_playing(3, 0); // current id0
    app.update(Msg::LyricsResult {
        video_id: "id0".to_owned(),
        lines: lyric_lines(),
    });
    assert!(
        app.lyrics
            .track
            .as_ref()
            .is_some_and(|l| l.lines.len() == 2)
    );
    // A late result for a different track is ignored.
    app.update(Msg::LyricsResult {
        video_id: "stale".to_owned(),
        lines: lyric_lines(),
    });
    assert_eq!(app.lyrics.track.as_ref().unwrap().video_id, "id0");
}

#[test]
fn advancing_track_clears_lyrics_and_refetches_when_open() {
    let mut app = app_playing(3, 0);
    app.lyrics.visible = true;
    app.update(Msg::LyricsResult {
        video_id: "id0".to_owned(),
        lines: lyric_lines(),
    });
    assert!(app.lyrics.track.is_some());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n')))); // -> id1
    assert!(app.lyrics.track.is_none());
    assert!(app.lyrics.loading);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::FetchLyrics { video_id, .. } if video_id == "id1"))
    );
}

// --- Album art ----------------------------------------------------------

#[test]
fn album_art_off_emits_no_fetch() {
    let mut app = app_playing(3, 0);
    // Opt-in: off by default → advancing a track issues no artwork fetch.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::FetchArtwork { .. })));
    assert!(!app.art.loading);
}

#[test]
fn album_art_on_fetches_remote_then_builds_protocol() {
    let mut app = app_playing(3, 0);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let (resize_tx, _) = tokio::sync::mpsc::unbounded_channel();
    app.set_art_resize_tx(resize_tx);
    // Advancing to id1 now fetches its thumbnail from the remote source.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(app.art.loading);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchArtwork { video_id, source: ArtSource::Remote { video_id: vid } }
            if video_id == "id1" && vid == "id1"
    )));
    // The decoded image becomes a render-ready protocol for the current track.
    app.update(Msg::ArtworkResult {
        video_id: "id1".to_owned(),
        image: Some(image::DynamicImage::new_rgb8(120, 120)),
    });
    assert!(!app.art.loading);
    assert!(app.art_active());
    assert_eq!(app.art.dims, (120, 120));
}

#[test]
fn artwork_result_for_stale_track_is_ignored() {
    let mut app = app_playing(3, 0); // current id0
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    app.update(Msg::ArtworkResult {
        video_id: "stale".to_owned(),
        image: Some(image::DynamicImage::new_rgb8(8, 8)),
    });
    assert!(!app.art_active());
}

#[test]
fn local_track_uses_local_art_source() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let song = Song::local_file(std::path::PathBuf::from("/music/song.m4a"));
    assert!(matches!(
        app.artwork_source(&song),
        Some(ArtSource::Local(_))
    ));
}

#[test]
fn art_fit_rect_centers_by_aspect() {
    let mut app = App::new(100);
    app.art.picker = Some(Picker::halfblocks()); // font cell 10x20 px
    app.art.dims = (100, 100); // square source
    let r = app.art_fit_rect(Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 40,
    });
    // Cells are 1:2 (10×20px), so a square cover spans the full width but only half the
    // height, centered vertically in the box.
    assert_eq!((r.width, r.height), (40, 20));
    assert_eq!((r.x, r.y), (0, 10));
}

// --- M7: downloads ------------------------------------------------------

#[test]
fn d_starts_download_of_current_track() {
    let mut app = app_playing(3, 0); // playing id0
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    match cmds.as_slice() {
        [Cmd::Download(song)] => assert_eq!(song.video_id, "id0"),
        _ => panic!("expected a Download cmd"),
    }
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Running(0))
    );
}

#[test]
fn d_ignores_local_tracks() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.queue.set(
        vec![Song::local_file(PathBuf::from("/tmp/local-track.m4a"))],
        0,
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(cmds.is_empty());
    assert!(app.status.text.contains("Already local"));
}

#[test]
fn download_progress_and_done_update_state() {
    let mut app = app_playing(1, 0);
    app.update(Msg::DownloadProgress {
        video_id: "id0".to_owned(),
        percent: 42.6,
    });
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Running(43))
    );
    app.update(Msg::DownloadDone {
        video_id: "id0".to_owned(),
        path: "/tmp/x.m4a".to_owned(),
    });
    assert_eq!(app.downloads.active.get("id0"), Some(&DownloadState::Done));
    assert!(app.status.text.contains("/tmp/x.m4a"));
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert_eq!(app.library_ui.downloaded[0].playback_target(), "/tmp/x.m4a");
}

#[test]
fn download_error_marks_failed() {
    let mut app = app_playing(1, 0);
    app.update(Msg::DownloadError {
        video_id: "id0".to_owned(),
        error: "boom".to_owned(),
    });
    assert_eq!(
        app.downloads.active.get("id0"),
        Some(&DownloadState::Failed)
    );
    assert!(app.status.text.contains("boom"));
}

// --- M8: prefetch / instant skip ----------------------------------------

fn resolve_cmd<'a>(cmds: &'a [Cmd], id: &str) -> Option<&'a str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Resolve {
            video_id,
            watch_url,
        } if video_id == id => Some(watch_url.as_str()),
        _ => None,
    })
}

#[test]
fn loading_prefetches_the_next_track() {
    // Loading id0 with id1 next in the queue → should request a resolve for id1.
    let mut app = App::new(100);
    app.queue.set(songs(3), 0);
    let song = app.queue.current().cloned();
    let cmds = app.load_song(song);
    assert!(resolve_cmd(&cmds, "id1").is_some_and(|u| u.contains("id1")));
}

#[test]
fn skip_uses_prefetched_url_when_available() {
    let mut app = app_playing(3, 0); // playing id0, prefetch requested for id1
    app.update(Msg::Resolved {
        video_id: "id1".to_owned(),
        stream_url: "https://cdn.example/stream-id1".to_owned(),
    });
    // Skip: id1 should load via the prefetched direct URL, not its watch URL.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    let url = load_url(&cmds).expect("a Load cmd");
    assert_eq!(url, "https://cdn.example/stream-id1");
    // And it should now prefetch id2.
    assert!(resolve_cmd(&cmds, "id2").is_some());
}

#[test]
fn skip_without_prefetch_falls_back_to_watch_url() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n')))); // no Resolved arrived
    let url = load_url(&cmds).expect("a Load cmd");
    assert!(url.contains("music.youtube.com/watch") && url.contains("id1"));
}

// --- M9: mouse controls -------------------------------------------------

#[test]
fn click_on_seekbar_seeks_to_fraction() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.bridges.seekbar_rect.set(Some(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    }));
    // Column 50 of a 100-wide bar → 50% of 200 s → ~100 s.
    let cmds = app.update(Msg::MouseClick { col: 50, row: 5 });
    match cmds.as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 100.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute cmd"),
    }
}

#[test]
fn click_off_seekbar_is_ignored() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.bridges.seekbar_rect.set(Some(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    }));
    assert!(app.update(Msg::MouseClick { col: 50, row: 9 }).is_empty()); // wrong row
    assert!(app.update(Msg::MouseClick { col: 200, row: 5 }).is_empty()); // past the bar
}

#[test]
fn click_does_nothing_outside_player_mode() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.bridges.seekbar_rect.set(Some(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    }));
    app.mode = Mode::Search;
    assert!(app.update(Msg::MouseClick { col: 50, row: 5 }).is_empty());
}

#[test]
fn click_player_buttons_dispatch_actions() {
    let mut app = app_playing(3, 0);
    app.register_mouse_button(
        Rect {
            x: 10,
            y: 4,
            width: 9,
            height: 1,
        },
        MouseTarget::Player(Action::TogglePause),
    );
    let cmds = app.update(Msg::MouseClick { col: 12, row: 4 });
    assert!(app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));

    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 22,
            y: 4,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::VolUp),
    );
    let cmds = app.update(Msg::MouseClick { col: 25, row: 4 });
    assert_eq!(app.playback.volume, 45);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(45))]
    ));
}

#[test]
fn wheel_over_volume_cluster_adjusts_volume_when_enabled() {
    let mut app = app_playing(1, 0);
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 20,
            y: 4,
            width: 16,
            height: 1,
        },
        MouseTarget::VolumeArea,
    );

    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: 25,
        row: 4,
    });
    assert_eq!(app.playback.volume, 45);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(45))]
    ));

    let cmds = app.update(Msg::MouseScroll {
        up: false,
        col: 25,
        row: 4,
    });
    assert_eq!(app.playback.volume, 40);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(40))]
    ));
}

#[test]
fn wheel_volume_setting_can_disable_volume_scroll() {
    let mut app = app_playing(1, 0);
    app.config.mouse_wheel_volume = Some(false);
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 20,
            y: 4,
            width: 16,
            height: 1,
        },
        MouseTarget::VolumeArea,
    );

    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: 25,
        row: 4,
    });
    assert!(cmds.is_empty());
    assert_eq!(app.playback.volume, 40);
}

#[test]
fn click_next_button_loads_next_track() {
    let mut app = app_playing(3, 0);
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 1,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::NextTrack),
    );
    let cmds = app.update(Msg::MouseClick { col: 3, row: 1 });
    assert_eq!(current(&app), "id1");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
}

#[test]
fn click_help_button_opens_cheatsheet() {
    let mut app = app_playing(1, 0);
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 9,
            width: 16,
            height: 1,
        },
        MouseTarget::Global(Action::ToggleHelp),
    );
    assert!(app.update(Msg::MouseClick { col: 4, row: 9 }).is_empty());
    assert!(app.help_visible);
}

#[test]
fn korean_q_key_closes_help_overlay() {
    let mut app = app_playing(1, 0);
    app.help_visible = true;
    assert!(app.update(Msg::Key(key(KeyCode::Char('ㅂ')))).is_empty());
    assert!(!app.help_visible);
}

#[test]
fn click_closes_help_overlay_before_buttons() {
    let mut app = app_playing(1, 0);
    app.help_visible = true;
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 1,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::VolUp),
    );
    assert!(app.update(Msg::MouseClick { col: 3, row: 1 }).is_empty());
    assert!(!app.help_visible);
    assert_eq!(app.playback.volume, 40);
}

fn rendered_help_button(app: &App, width: u16, height: u16) -> MouseButtonRegion {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();

    app.bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::Global(Action::ToggleHelp))
        .copied()
        .expect("rendered help button")
}

#[test]
fn library_scrollbar_shows_only_when_the_list_overflows() {
    // The thumb glyph appears on the right border column (79 in an 80-wide frame); the
    // plain vertical border is a different glyph, so its presence proves the scrollbar.
    let has_thumb = |app: &App| -> bool {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..20).any(|y| buf.cell((79, y)).is_some_and(|c| c.symbol() == "█"))
    };

    let mut overflow = App::new(100);
    for i in 0..40 {
        overflow.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    overflow.mode = Mode::Library;
    overflow.library_ui.tab = LibraryTab::History;
    assert!(
        has_thumb(&overflow),
        "a long list should show a scrollbar thumb"
    );

    let mut fits = App::new(100);
    fits.library
        .record_play(&Song::remote("a", "ta", "x", "0:10"));
    fits.library
        .record_play(&Song::remote("b", "tb", "x", "0:10"));
    fits.mode = Mode::Library;
    fits.library_ui.tab = LibraryTab::History;
    assert!(
        !has_thumb(&fits),
        "a short list should not show a scrollbar"
    );
}

#[test]
fn library_scrollbar_thumb_tracks_the_actual_page_offset() {
    let mut app = App::new(100);
    for i in 0..40 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    app.bridges
        .library_scroll
        .wheel(false, 999, app.library_len());
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buf = terminal.backend().buffer();
    assert_eq!(
        buf.cell((79, 17)).map(|c| c.symbol()),
        Some("█"),
        "at the final page the scrollbar thumb should touch the list bottom"
    );
}

#[test]
fn dragging_library_scrollbar_moves_the_viewport() {
    let mut app = App::new(100);
    for i in 0..40 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::Library))
        .map(|b| b.rect)
        .expect("rendered library scrollbar rect");

    app.update(Msg::MouseClick {
        col: bar.x,
        row: bar.y,
    });
    app.update(Msg::MouseDrag {
        col: bar.x,
        row: bar.y + bar.height - 1,
    });

    assert_eq!(
        app.bridges.library_scroll.offset(),
        app.library_len() - app.bridges.library_scroll.viewport()
    );
}

#[test]
fn dragging_search_scrollbar_moves_the_viewport() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(40);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::Search))
        .map(|b| b.rect)
        .expect("rendered search scrollbar rect");

    app.update(Msg::MouseClick {
        col: bar.x,
        row: bar.y,
    });
    app.update(Msg::MouseDrag {
        col: bar.x,
        row: bar.y + bar.height - 1,
    });

    assert_eq!(
        app.bridges.search_scroll.offset(),
        app.search.results.len() - app.bridges.search_scroll.viewport()
    );
}

#[test]
fn ai_suggestions_scrollbar_renders_inside_borderless_list() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "hide onboarding art".to_owned(),
    });
    app.ai.suggestions = songs(20);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::AiSuggestions))
        .map(|b| b.rect)
        .expect("rendered DJ Gem suggestions scrollbar rect");
    let buf = terminal.backend().buffer();

    assert!(bar.x < 80, "scrollbar should not be registered off-screen");
    assert!(
        (bar.y..bar.y + bar.height).any(|y| {
            buf.cell((bar.x, y))
                .is_some_and(|c| matches!(c.symbol(), "█" | "│"))
        }),
        "scrollbar should be drawn at its registered hit rect"
    );
}

fn assert_centered_in(rect: Rect, container: Rect) {
    let left = rect.x.saturating_sub(container.x);
    let right = container
        .x
        .saturating_add(container.width)
        .saturating_sub(rect.x.saturating_add(rect.width));
    assert_eq!(
        left, right,
        "help button should be centered in {container:?}"
    );
}

#[test]
fn help_button_is_centered_on_footer_screens() {
    let inner = Rect {
        x: 1,
        y: 1,
        width: 78,
        height: 18,
    };

    let player = App::new(100);
    assert_centered_in(rendered_help_button(&player, 80, 20).rect, inner);

    let mut search = App::new(100);
    search.mode = Mode::Search;
    assert_centered_in(rendered_help_button(&search, 80, 20).rect, inner);

    let mut library = App::new(100);
    library.mode = Mode::Library;
    assert_centered_in(rendered_help_button(&library, 80, 20).rect, inner);

    let mut ai = App::new(100);
    ai.mode = Mode::Ai;
    assert_centered_in(rendered_help_button(&ai, 80, 20).rect, inner);
}

#[test]
fn rating_key_cycles_neutral_like_dislike() {
    let mut app = app_playing(2, 0);
    let id = current(&app).to_owned();
    // Starts neutral: neither favorited nor disliked.
    assert!(!app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
    // First `f` → like (favorite); persists both library and signals.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    // Second `f` → dislike; flips the flag and drops the favorite.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(!app.library.is_favorite(&id));
    assert!(app.signals.is_disliked(&id));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    // Third `f` → back to neutral.
    app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(!app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
}

#[test]
fn rating_radio_toggles_radio_favorite_without_signals() {
    let mut app = App::new(100);
    let station = radio_station("station-like");
    app.queue.set(vec![station], 0);
    app.mode = Mode::Player;
    app.load_song(app.queue.current().cloned());

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));

    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert!(app.library.favorites.is_empty());
    assert!(app.library.is_radio_favorite("rad:station-like"));
    assert!(!app.signals.is_disliked("rad:station-like"));
    assert_eq!(
        app.library.radios.front().map(|s| s.video_id.as_str()),
        Some("rad:station-like")
    );

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert_eq!(row_ids(&app), vec!["rad:station-like"]);
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());

    app.mode = Mode::Player;
    app.queue.set(vec![radio_station("station-like")], 0);
    app.load_song(app.queue.current().cloned());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert!(!app.library.is_radio_favorite("rad:station-like"));

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Radio;
    assert_eq!(row_ids(&app), vec!["rad:station-like"]);
}

#[test]
fn manual_next_records_signals_then_advances() {
    let mut app = app_playing(3, 0);
    let id = current(&app).to_owned();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    // The skipped track is persisted (SaveSignals) and playback advances.
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert_ne!(current(&app), id);
}

#[test]
fn manual_next_from_radio_does_not_record_signals() {
    let mut app = App::new(100);
    app.queue.set(
        vec![
            radio_station("station-skip"),
            Song::remote("id0", "t0", "a", "0:10"),
        ],
        0,
    );
    app.mode = Mode::Player;
    app.load_song(app.queue.current().cloned());

    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));

    assert!(!cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert_eq!(current(&app), "id0");
}

#[test]
fn eof_records_signals_for_the_finished_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::PlayerEof);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
}

#[test]
fn rendering_player_registers_control_buttons() {
    let app = app_playing(2, 0);
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buttons = app.bridges.mouse_buttons.borrow();
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::TogglePause))
    );
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::PrevTrack))
    );
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::NextTrack))
    );
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::VolDown))
    );
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::VolUp))
    );
    assert!(buttons.iter().any(|b| b.target == MouseTarget::VolumeArea));
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Global(Action::ToggleHelp))
    );
    // The status line publishes the shuffle + repeat toggles and the EQ-dropdown opener.
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::ToggleShuffle))
    );
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::CycleRepeat))
    );
    assert!(buttons.iter().any(|b| b.target == MouseTarget::EqMenu));
    // The single tri-state rating control for the current track sits on the status line.
    assert!(
        buttons
            .iter()
            .any(|b| b.target == MouseTarget::Player(Action::CycleRating))
    );
    assert!(app.bridges.seekbar_rect.get().is_some());
}

#[test]
fn rendering_settings_registers_clickable_controls() {
    // Each control kind must publish its own hit target *on top of* the row-select rect, so a
    // click changes/activates the value rather than only moving the cursor onto it.
    let render_targets = |tab: SettingsTab| -> Vec<MouseTarget> {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (mode → Settings)
        app.settings.as_mut().unwrap().tab = tab;
        let backend = TestBackend::new(80, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
        app.bridges
            .mouse_buttons
            .borrow()
            .iter()
            .map(|b| b.target)
            .collect()
    };

    // Graphics: a Toggle (RetroMode, field 0), a Select (ThemePreset, field 1), a Toggle
    // (BackgroundNone, field 2), and a Text color row (first ThemeColor, field 3).
    let g = render_targets(SettingsTab::Graphics);
    let has = |ts: &[MouseTarget], t: MouseTarget| ts.contains(&t);
    assert!(
        has(&g, MouseTarget::SettingsChange { row: 0, delta: 1 }),
        "retro mode toggle"
    );
    assert!(
        has(&g, MouseTarget::SettingsChange { row: 1, delta: -1 }),
        "preset ‹ arrow"
    );
    assert!(
        has(&g, MouseTarget::SettingsChange { row: 1, delta: 1 }),
        "preset › arrow"
    );
    assert!(
        has(&g, MouseTarget::SettingsChange { row: 2, delta: 1 }),
        "background toggle"
    );
    assert!(
        has(&g, MouseTarget::SettingsActivate(3)),
        "color row enters hex editor"
    );
    // Headers are render-only — a click on one falls through to nothing, never a field.

    // Playback leads with the Speed slider (field 0): its ‹ › step arrows are click targets.
    let p = render_targets(SettingsTab::Playback);
    assert!(
        has(&p, MouseTarget::SettingsChange { row: 0, delta: -1 }),
        "speed ‹ arrow"
    );
    assert!(
        has(&p, MouseTarget::SettingsChange { row: 0, delta: 1 }),
        "speed › arrow"
    );

    // General's Reset buttons (no value) activate on click.
    let general = render_targets(SettingsTab::General);
    let reset_all = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ResetAll)
        .unwrap();
    assert!(
        has(&general, MouseTarget::SettingsActivate(reset_all)),
        "reset-all button"
    );
}

#[test]
fn settings_control_hit_rects_land_on_their_glyphs() {
    // The strongest guard against the per-control rect math drifting from what `field_row`
    // actually draws: assert each registered rect's top-left cell holds the glyph it targets.
    // If the gutter/label-width offsets were wrong, the arrow rects would miss the glyphs.
    let cell_at = |tab: SettingsTab, want: MouseTarget| -> String {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
        app.settings.as_mut().unwrap().tab = tab;
        let backend = TestBackend::new(80, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
        let rect = app
            .bridges
            .mouse_buttons
            .borrow()
            .iter()
            .find(|b| b.target == want)
            .map(|b| b.rect)
            .unwrap_or_else(|| panic!("no rect registered for {want:?}"));
        let buf = terminal.backend().buffer().clone();
        buf.cell((rect.x, rect.y))
            .map(|c| c.symbol().to_owned())
            .unwrap_or_default()
    };

    // Speed slider (Playback field 0): the −/+ rects sit on the ‹ › step arrows.
    let dec = MouseTarget::SettingsChange { row: 0, delta: -1 };
    let inc = MouseTarget::SettingsChange { row: 0, delta: 1 };
    assert_eq!(
        cell_at(SettingsTab::Playback, dec),
        "‹",
        "speed decrease lands on ‹"
    );
    assert_eq!(
        cell_at(SettingsTab::Playback, inc),
        "›",
        "speed increase lands on ›"
    );
    // ThemePreset (Graphics field 1): a Select, so the arrows are < >.
    let theme_dec = MouseTarget::SettingsChange { row: 1, delta: -1 };
    let theme_inc = MouseTarget::SettingsChange { row: 1, delta: 1 };
    assert_eq!(
        cell_at(SettingsTab::Graphics, theme_dec),
        "<",
        "preset decrease lands on <"
    );
    assert_eq!(
        cell_at(SettingsTab::Graphics, theme_inc),
        ">",
        "preset increase lands on >"
    );
    // BackgroundNone (Graphics field 2): a Toggle, rect over the [ ] / [x] checkbox.
    let toggle = MouseTarget::SettingsChange { row: 2, delta: 1 };
    assert_eq!(
        cell_at(SettingsTab::Graphics, toggle),
        "[",
        "background toggle lands on ["
    );
}

#[test]
fn eq_dropdown_renders_preset_rows_when_open() {
    let mut app = app_playing(2, 0);
    app.dropdowns.eq_open = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buttons = app.bridges.mouse_buttons.borrow();
    // One selectable row per built-in preset.
    for preset in crate::eq::EqPreset::CYCLE {
        assert!(
            buttons
                .iter()
                .any(|b| b.target == MouseTarget::EqSelect(preset)),
            "missing dropdown row for {preset:?}"
        );
    }
}

#[test]
fn clicking_eq_label_toggles_dropdown() {
    let mut app = app_playing(1, 0);
    app.register_mouse_button(
        Rect {
            x: 30,
            y: 4,
            width: 7,
            height: 1,
        },
        MouseTarget::EqMenu,
    );
    assert!(app.update(Msg::MouseClick { col: 32, row: 4 }).is_empty());
    assert!(app.dropdowns.eq_open);
    // Clicking it again closes it.
    app.register_mouse_button(
        Rect {
            x: 30,
            y: 4,
            width: 7,
            height: 1,
        },
        MouseTarget::EqMenu,
    );
    assert!(app.update(Msg::MouseClick { col: 32, row: 4 }).is_empty());
    assert!(!app.dropdowns.eq_open);
}

#[test]
fn selecting_eq_preset_applies_and_closes_dropdown() {
    let mut app = app_playing(1, 0);
    app.dropdowns.eq_open = true;
    app.register_mouse_button(
        Rect {
            x: 30,
            y: 6,
            width: 12,
            height: 1,
        },
        MouseTarget::EqSelect(EqPreset::Vocal),
    );
    let cmds = app.update(Msg::MouseClick { col: 33, row: 6 });
    assert_eq!(app.audio.preset, EqPreset::Vocal);
    assert_eq!(app.audio.bands, EqPreset::Vocal.gains());
    assert!(!app.dropdowns.eq_open);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetAudioFilter(_))]
    ));
}

#[test]
fn outside_click_dismisses_eq_dropdown_without_seeking() {
    let mut app = app_playing(1, 0);
    app.dropdowns.eq_open = true;
    app.playback.duration = Some(200.0);
    app.bridges.seekbar_rect.set(Some(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    }));
    // A click on the seekbar with the dropdown open just closes it (no seek emitted).
    let cmds = app.update(Msg::MouseClick { col: 50, row: 5 });
    assert!(!app.dropdowns.eq_open);
    assert!(cmds.is_empty());
}

#[test]
fn art_overlay_mask_tracks_each_popup_independently() {
    // The render loop clears native terminal graphics on any change to this mask, so every
    // art-covering surface needs its own bit — switching one straight to another, or stacking a
    // second over a first, must register as an edge.
    let mut app = app_playing(1, 0);
    assert_eq!(app.art_overlay_mask(), 0);
    app.dropdowns.eq_open = true;
    assert_eq!(app.art_overlay_mask(), 1 << 0);
    // Switch eq -> radio: the mask still changes even though some popup
    // stays open across the switch.
    app.dropdowns.eq_open = false;
    app.dropdowns.radio_open = true;
    assert_eq!(app.art_overlay_mask(), 1 << 1);
    // The queue window is a distinct bit, and can stack with a dropdown.
    app.queue_popup.open = true;
    assert_eq!(app.art_overlay_mask(), (1 << 1) | (1 << 2));
    app.dropdowns.radio_open = false;
    assert_eq!(app.art_overlay_mask(), 1 << 2);
    app.queue_popup.open = false;
    assert_eq!(app.art_overlay_mask(), 0);

    app.help_visible = true;
    assert_eq!(app.art_overlay_mask(), 1 << 3);
    app.help_visible = false;
    app.about_visible = true;
    assert_eq!(app.art_overlay_mask(), 1 << 4);
    app.about_visible = false;
    app.why_ai_visible = true;
    assert_eq!(app.art_overlay_mask(), 1 << 5);
    app.why_ai_visible = false;
    app.key_conflict = Some(Conflict {
        ctx: KeyContext::Player,
        existing: Action::TogglePause,
        chord: Chord::new(KeyCode::Char('x'), KeyModifiers::NONE),
    });
    assert_eq!(app.art_overlay_mask(), 1 << 6);
    app.key_conflict = None;
    app.pending_radio_mode_confirm = Some(RadioModeConfirm::Enter);
    assert_eq!(app.art_overlay_mask(), 1 << 7);
    app.pending_radio_mode_confirm = None;
    app.pending_settings_confirm = Some(SettingsConfirm::ResetAll);
    assert_eq!(app.art_overlay_mask(), 1 << 8);
    app.pending_settings_confirm = None;
    app.library_ui.confirm_delete = Some(vec![std::path::PathBuf::from("track.mp3")]);
    assert_eq!(app.art_overlay_mask(), 1 << 9);
    app.library_ui.confirm_delete = None;
    app.mode = Mode::Search;
    assert_eq!(app.art_overlay_mask(), 1 << 10);
    app.mode = Mode::Player;
    assert_eq!(app.art_overlay_mask(), 0);
}

fn configure_test_art_picker(app: &mut App, protocol: ratatui_image::picker::ProtocolType) {
    let mut picker = ratatui_image::picker::Picker::halfblocks();
    picker.set_protocol_type(protocol);
    app.config.album_art = Some(true);
    app.art.picker = Some(picker);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_art_resize_tx(tx);
}

fn make_test_art_active(app: &mut App, protocol: ratatui_image::picker::ProtocolType) {
    configure_test_art_picker(app, protocol);
    let video_id = app.queue.current().unwrap().video_id.clone();
    app.set_artwork(video_id, Some(image::DynamicImage::new_rgba8(32, 32)));
    app.art.overlay_mask = app.art_overlay_mask();
    app.art.force_clear_next_frame = false;
    app.art.overlay_refresh_clear_frames = 0;
    app.dirty = false;
}

fn assert_art_refresh_clear_burst(app: &mut App, context: &str) {
    for frame in 1..=3 {
        assert!(
            app.clear_before_draw_pending(),
            "{context}: pending flag should keep redraw loop awake before frame {frame}"
        );
        assert!(
            app.take_clear_before_draw(),
            "{context}: expected reinforced clear frame {frame}"
        );
    }
    assert!(
        !app.clear_before_draw_pending(),
        "{context}: pending flag should drop after the burst"
    );
    assert!(
        !app.take_clear_before_draw(),
        "{context}: reinforced clear burst should be short"
    );
}

#[test]
fn named_overlay_transitions_request_one_full_clear_when_native_art_is_active() {
    fn set_eq(app: &mut App, open: bool) {
        app.dropdowns.eq_open = open;
    }
    fn set_radio(app: &mut App, open: bool) {
        app.dropdowns.radio_open = open;
    }
    fn set_queue(app: &mut App, open: bool) {
        app.queue_popup.open = open;
    }
    fn set_about(app: &mut App, open: bool) {
        app.about_visible = open;
    }
    fn set_why_ai(app: &mut App, open: bool) {
        app.why_ai_visible = open;
    }

    for (name, set_open) in [
        ("eq dropdown", set_eq as fn(&mut App, bool)),
        ("radio dropdown", set_radio),
        ("queue popup", set_queue),
        ("about popup", set_about),
        ("why-ai popup", set_why_ai),
    ] {
        let mut app = app_playing(1, 0);
        make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);

        set_open(&mut app, true);
        app.update(Msg::Resize);
        assert!(
            app.take_clear_before_draw(),
            "{name} opening should clear native art before redraw"
        );
        assert!(
            !app.take_clear_before_draw(),
            "{name} opening clear request should be one-shot"
        );

        set_open(&mut app, false);
        app.update(Msg::Resize);
        assert!(
            app.take_clear_before_draw(),
            "{name} closing should clear native art before redraw"
        );
        assert!(
            !app.take_clear_before_draw(),
            "{name} closing clear request should be one-shot"
        );
    }
}

#[test]
fn art_overlay_transition_does_not_clear_without_art() {
    let mut app = app_playing(1, 0);

    app.about_visible = true;
    app.update(Msg::Resize);
    assert!(!app.take_clear_before_draw());
}

#[test]
fn art_overlay_transition_does_not_clear_for_halfblocks_art() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);

    app.about_visible = true;
    app.update(Msg::Resize);
    assert!(!app.take_clear_before_draw());
}

#[test]
fn about_native_icon_transition_requests_clear_without_album_art() {
    let mut app = app_playing(1, 0);
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Sixel);

    app.about_visible = true;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw());
    assert!(!app.take_clear_before_draw());

    app.about_visible = false;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw());
}

#[test]
fn artwork_arriving_under_overlay_requests_full_clear() {
    let mut app = app_playing(1, 0);
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Sixel);
    app.about_visible = true;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw());

    let video_id = app.queue.current().unwrap().video_id.clone();
    app.set_artwork(video_id, Some(image::DynamicImage::new_rgba8(32, 32)));
    assert_art_refresh_clear_burst(&mut app, "artwork arriving under overlay");
}

#[test]
fn artwork_resize_completion_under_overlay_reinforces_overlay() {
    let mut app = app_playing(1, 0);
    let mut picker = ratatui_image::picker::Picker::halfblocks();
    picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
    app.config.album_art = Some(true);
    app.art.picker = Some(picker);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.set_art_resize_tx(tx);

    let video_id = app.queue.current().unwrap().video_id.clone();
    app.set_artwork(video_id, Some(image::DynamicImage::new_rgba8(32, 32)));
    app.queue_popup.open = true;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw(), "opening overlay clears once");
    assert!(
        !app.take_clear_before_draw(),
        "overlay opening stays one-shot"
    );

    render_app(&app);
    let request = rx
        .try_recv()
        .expect("rendering pending artwork should request resize/encode");
    app.apply_artwork_resize(request.resize_encode().unwrap());

    assert_art_refresh_clear_burst(&mut app, "artwork resize completion under overlay");
}

#[test]
fn current_queue_delete_under_overlay_requests_clear_for_removed_native_art() {
    let mut app = app_playing(3, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);

    app.queue_popup.open = true;
    app.update(Msg::Resize);
    assert!(
        app.take_clear_before_draw(),
        "opening the queue popup clears once"
    );
    app.dirty = false;

    let cmds = app.remove_queue_range(0, 0);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("id1")
    );
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchArtwork { video_id, .. } if video_id == "id1"
    )));
    assert_art_refresh_clear_burst(
        &mut app,
        "removing the visible native art under the queue popup",
    );
}

#[test]
fn deleting_last_queue_track_under_overlay_clears_native_art() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);

    app.queue_popup.open = true;
    app.update(Msg::Resize);
    assert!(
        app.take_clear_before_draw(),
        "opening the queue popup clears once"
    );
    app.dirty = false;

    let cmds = app.remove_queue_range(0, 0);
    assert!(has_stop(&cmds));
    assert!(app.queue.is_empty());
    assert!(!app.art_active());
    assert_art_refresh_clear_burst(&mut app, "emptying the queue under overlay");
}

#[test]
fn native_art_clear_under_player_overlays_requests_full_clear() {
    fn set_eq(app: &mut App, open: bool) {
        app.dropdowns.eq_open = open;
    }
    fn set_radio(app: &mut App, open: bool) {
        app.dropdowns.radio_open = open;
    }
    fn set_help(app: &mut App, open: bool) {
        app.help_visible = open;
    }
    fn set_about(app: &mut App, open: bool) {
        app.about_visible = open;
    }

    for (name, set_open) in [
        ("eq dropdown", set_eq as fn(&mut App, bool)),
        ("radio dropdown", set_radio),
        ("help overlay", set_help),
        ("about popup", set_about),
    ] {
        let mut app = app_playing(3, 0);
        make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);

        set_open(&mut app, true);
        app.update(Msg::Resize);
        assert!(app.take_clear_before_draw(), "{name} opening clears once");
        app.dirty = false;

        let cmds = app.advance(false);
        assert_eq!(
            app.queue.current().map(|s| s.video_id.as_str()),
            Some("id1")
        );
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::FetchArtwork { video_id, .. } if video_id == "id1"
        )));
        assert_art_refresh_clear_burst(&mut app, &format!("{name}: track change under overlay"));
    }
}

#[test]
fn clearing_halfblocks_art_under_overlay_does_not_request_native_clear() {
    let mut app = app_playing(3, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);
    app.queue_popup.open = true;
    app.art.overlay_mask = app.art_overlay_mask();
    app.art.force_clear_next_frame = false;
    app.art.overlay_refresh_clear_frames = 0;
    app.dirty = false;

    let _ = app.remove_queue_range(0, 0);
    assert!(!app.take_clear_before_draw());
}

#[test]
fn popup_surfaces_render_opaque_backgrounds_with_transparent_theme() {
    let player_area = ratatui::layout::Rect::new(0, 0, 80, 20);
    let modal_area = ratatui::layout::Rect::new(0, 0, 80, 24);

    let mut eq = app_playing(3, 0);
    eq.dropdowns.eq_open = true;
    let buf = render_app_buffer(&eq, player_area.width, player_area.height);
    assert_opaque_rect(
        &buf,
        dropdown_popup_rect(&eq, |t| matches!(t, MouseTarget::EqSelect(_))),
    );

    let mut radio = app_playing(3, 0);
    radio.autoplay_radio = true;
    radio.dropdowns.radio_open = true;
    let buf = render_app_buffer(&radio, player_area.width, player_area.height);
    assert_opaque_rect(
        &buf,
        dropdown_popup_rect(&radio, |t| matches!(t, MouseTarget::RadioSelect(_))),
    );

    let mut queue = app_playing(5, 0);
    queue.open_queue_popup();
    let buf = render_app_buffer(&queue, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, queue.queue_popup.rect.get().unwrap());

    let mut help = app_playing(1, 0);
    help.help_visible = true;
    let buf = render_app_buffer(&help, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_percent(modal_area, 80, 80));

    let mut about = app_playing(1, 0);
    about.about_visible = true;
    let buf = render_app_buffer(&about, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 60, 22));

    let mut why = app_playing(2, 0);
    why.radio.last_explain = Some(RadioAiExplain {
        conf: Some(0.82),
        picks: vec![
            ExplainPick {
                title: "Bridge Track".to_owned(),
                artist: "Some Artist".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned()],
            },
            ExplainPick {
                title: "Core Track".to_owned(),
                artist: "Another Artist".to_owned(),
                role: Some("core".to_owned()),
                reasons: vec![],
            },
        ],
    });
    why.why_ai_visible = true;
    let buf = render_app_buffer(&why, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 72, 9));

    let mut conflict = app_playing(1, 0);
    conflict.key_conflict = Some(Conflict {
        ctx: KeyContext::Player,
        existing: Action::TogglePause,
        chord: Chord::new(KeyCode::Char('x'), KeyModifiers::NONE),
    });
    let buf = render_app_buffer(&conflict, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 54, 9));

    let mut reset = app_playing(1, 0);
    reset.pending_settings_confirm = Some(SettingsConfirm::ResetAll);
    let buf = render_app_buffer(&reset, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 56, 9));

    let mut delete = app_playing(1, 0);
    delete.library_ui.confirm_delete = Some(vec![std::path::PathBuf::from("track.mp3")]);
    let buf = render_app_buffer(&delete, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 56, 9));
}

#[test]
fn about_icon_composites_transparent_pixels_against_popup_background() {
    let area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let icon = about_icon_rect(area);
    let mut app = app_playing(1, 0);
    app.about_visible = true;
    app.theme
        .set_override(crate::theme::ThemeRole::Background, "#123456")
        .unwrap();

    let buf = render_app_buffer(&app, area.width, area.height);
    assert_rgb_at_least(
        buf.cell((icon.left(), icon.top()))
            .expect("icon top-left is inside the buffer")
            .bg,
        (0x12, 0x34, 0x56),
    );

    app.theme
        .set_override(crate::theme::ThemeRole::Background, "#654321")
        .unwrap();
    let buf = render_app_buffer(&app, area.width, area.height);
    assert_rgb_at_least(
        buf.cell((icon.left(), icon.top()))
            .expect("icon top-left is inside the buffer")
            .bg,
        (0x65, 0x43, 0x21),
    );
}

#[test]
fn about_icon_uses_foreground_kitty_when_available() {
    use ratatui_image::picker::{Picker, ProtocolType};

    let area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let icon = about_icon_rect(area);
    let mut app = app_playing(1, 0);
    app.about_visible = true;

    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    app.art.picker = Some(picker);

    let buf = render_app_buffer(&app, area.width, area.height);
    let cached_protocol = app
        .about_icon
        .borrow()
        .as_ref()
        .map(|(_, protocol, _)| *protocol);
    assert_eq!(cached_protocol, Some(Some(ProtocolType::Kitty)));

    let symbol = buf
        .cell((icon.left(), icon.top()))
        .expect("icon top-left is inside the buffer")
        .symbol();
    assert!(symbol.contains("_G"));
    assert!(symbol.contains("z=0,"));
}

#[test]
fn about_icon_uses_sixel_when_available() {
    use ratatui_image::picker::{Picker, ProtocolType};

    let area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let icon = about_icon_rect(area);
    let mut app = app_playing(1, 0);
    app.about_visible = true;

    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Sixel);
    app.art.picker = Some(picker);

    let buf = render_app_buffer(&app, area.width, area.height);
    let cached_protocol = app
        .about_icon
        .borrow()
        .as_ref()
        .map(|(_, protocol, _)| *protocol);
    assert_eq!(cached_protocol, Some(Some(ProtocolType::Sixel)));

    let symbol = buf
        .cell((icon.left(), icon.top()))
        .expect("icon top-left is inside the buffer")
        .symbol();
    assert!(symbol.contains("\x1bP"));
}

#[test]
fn popup_art_marker_leaves_current_player_anchor_unchanged() {
    use ratatui_image::protocol::kitty::StatefulKitty;
    use ratatui_image::protocol::{StatefulProtocol, StatefulProtocolType};
    use ratatui_image::{FontSize, Resize, ResizeEncodeRender};

    let app = app_playing(1, 0);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut protocol = StatefulProtocol::new(
        image::DynamicImage::new_rgba8(10, 10),
        FontSize::new(10, 20),
        None,
        StatefulProtocolType::Kitty(StatefulKitty::new(42, false)),
    );
    protocol.resize_encode(&Resize::Scale(None), ratatui::layout::Size::new(5, 3));
    *app.art.protocol.borrow_mut() = Some(ThreadProtocol::new(tx, Some(protocol)));

    let art = Rect::new(2, 1, 5, 3);
    let popup = Rect::new(art.left() + 2, art.top() + 1, 2, 1);
    app.art.rect.set(Some(art));

    let backend = TestBackend::new(12, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let anchor = (art.left(), art.top() + 1);
            let before = frame
                .buffer_mut()
                .cell(anchor)
                .expect("anchor is inside the buffer")
                .symbol()
                .to_owned();

            crate::ui::mark_art_rows_for_popup(frame, &app, popup);

            let after = frame
                .buffer_mut()
                .cell(anchor)
                .expect("anchor is inside the buffer")
                .symbol()
                .to_owned();
            assert_eq!(before, after);
        })
        .unwrap();
}

#[test]
fn player_about_popup_keeps_full_kitty_art_rows_at_the_edges() {
    use image::imageops::FilterType;
    use ratatui_image::picker::ProtocolType;
    use ratatui_image::{Resize, ResizeEncodeRender};

    let area = Rect::new(0, 0, 120, 50);
    let popup = centered_fixed(area, 60, 22);
    let image = image::DynamicImage::new_rgba8(160, 90);
    let mut app = app_playing(1, 0);
    app.about_visible = true;
    configure_test_art_picker(&mut app, ProtocolType::Kitty);
    let video_id = app.queue.current().unwrap().video_id.clone();
    app.set_artwork(video_id, Some(image.clone()));

    // First render publishes the actual fitted art rect. The protocol may go pending because the
    // resize worker is absent in this unit test; we only need the rect for the deterministic setup
    // below.
    let _ = render_app_buffer(&app, area.width, area.height);
    let art = app
        .art
        .rect
        .get()
        .expect("player render should publish art rect");
    assert!(
        art.left() < popup.left(),
        "test geometry must expose a left art edge"
    );
    assert!(
        !art.intersection(popup).is_empty(),
        "test geometry must overlap the About popup"
    );

    let mut protocol = app
        .art
        .picker
        .as_ref()
        .expect("configured above")
        .new_resize_protocol(image);
    protocol.resize_encode(
        &Resize::Scale(Some(FilterType::Lanczos3)),
        ratatui::layout::Size::new(art.width, art.height),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    *app.art.protocol.borrow_mut() = Some(ThreadProtocol::new(tx, Some(protocol)));

    let buf = render_app_buffer(&app, area.width, area.height);
    let overlap = art.intersection(popup);
    for y in overlap.top()..overlap.bottom() {
        let symbol = buf
            .cell((art.left(), y))
            .expect("art anchor row is inside the buffer")
            .symbol();
        let placeholders = symbol.chars().filter(|ch| *ch == '\u{10EEEE}').count();
        assert!(
            placeholders > 1,
            "row {y} was replaced by a single Kitty marker instead of the full art row: {symbol:?}"
        );
    }
}

#[test]
fn non_player_frame_clears_stale_art_rect_so_popups_dont_bleed_art() {
    // Album art is only drawn by the player view, which records its rect in `app.art.rect`. On any
    // other screen (Search, Library, ...) the art isn't on screen, yet its kitty image is still
    // transmitted to the terminal. A full frame must clear `art.rect` up front so a leftover rect
    // from the last player frame can't survive — otherwise `mark_art_rows_for_popup` (run by every
    // popup, e.g. About) re-anchors that stale image under the popup and bleeds it through as a
    // stray vertical bar.
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.about_visible = true;
    // A leftover rect from the last time the player view was shown.
    app.art.rect.set(Some(Rect::new(4, 3, 10, 6)));

    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    assert_eq!(
        app.art.rect.get(),
        None,
        "a non-player frame must clear the stale album-art rect before popups read it"
    );
}

#[test]
fn search_mode_popup_does_not_replant_stale_album_art_placeholder() {
    // End-to-end guard for the stray vertical bar: render a real (already-transmitted) kitty image
    // protocol with a stale player-era art rect, on the Search screen, with the About card open. A
    // full frame must not re-plant any kitty unicode placeholder — the player view didn't run, so
    // its art has no business reappearing under/beside the popup.
    use ratatui_image::protocol::kitty::StatefulKitty;
    use ratatui_image::protocol::{StatefulProtocol, StatefulProtocolType};
    use ratatui_image::{FontSize, Resize, ResizeEncodeRender};

    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.about_visible = true;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut protocol = StatefulProtocol::new(
        image::DynamicImage::new_rgba8(20, 20),
        FontSize::new(10, 20),
        None,
        StatefulProtocolType::Kitty(StatefulKitty::new(42, false)),
    );
    protocol.resize_encode(&Resize::Scale(None), ratatui::layout::Size::new(20, 8));
    *app.art.protocol.borrow_mut() = Some(ThreadProtocol::new(tx, Some(protocol)));

    // A leftover rect whose left edge sits just outside the centered About card but overlaps it
    // vertically — the exact geometry that re-anchored art col-0 as a stray bar beside the popup.
    // (Without the per-frame clear, `mark_art_rows_for_popup` would plant placeholders at column 2.)
    app.art.rect.set(Some(Rect::new(2, 6, 20, 8)));

    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    // The About icon falls back to half-blocks here (no graphics picker), so it contributes no
    // placeholder — any `\u{10EEEE}` in the buffer would be the stale art bleeding through.
    let buf = terminal.backend().buffer();
    let mut planted = Vec::new();
    for y in 0..30u16 {
        for x in 0..80u16 {
            if buf[(x, y)].symbol().contains('\u{10EEEE}') {
                planted.push((x, y));
            }
        }
    }
    assert!(
        planted.is_empty(),
        "stale album art re-planted as kitty placeholders at {planted:?}"
    );
}

#[test]
fn rendering_player_registers_radio_menu_when_autoplay_on() {
    let mut app = app_playing(2, 0);
    app.autoplay_radio = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    assert!(
        app.bridges
            .mouse_buttons
            .borrow()
            .iter()
            .any(|b| b.target == MouseTarget::RadioMenu)
    );
}

#[test]
fn radio_dropdown_renders_mode_rows_when_open() {
    let mut app = app_playing(2, 0);
    app.autoplay_radio = true;
    app.dropdowns.radio_open = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buttons = app.bridges.mouse_buttons.borrow();
    for mode in crate::radio::RadioMode::CYCLE {
        assert!(
            buttons
                .iter()
                .any(|b| b.target == MouseTarget::RadioSelect(mode)),
            "missing dropdown row for {mode:?}"
        );
    }
}

#[test]
fn clicking_radio_label_closes_eq_and_opens_radio_dropdown() {
    let mut app = app_playing(1, 0);
    // Open the EQ dropdown first to prove the two are mutually exclusive.
    app.dropdowns.eq_open = true;
    app.register_mouse_button(
        Rect {
            x: 40,
            y: 4,
            width: 14,
            height: 1,
        },
        MouseTarget::RadioMenu,
    );
    assert!(app.update(Msg::MouseClick { col: 42, row: 4 }).is_empty());
    assert!(app.dropdowns.radio_open);
    assert!(!app.dropdowns.eq_open);
}

#[test]
fn selecting_radio_mode_applies_and_persists() {
    use crate::radio::RadioMode;
    let mut app = app_playing(1, 0);
    app.dropdowns.radio_open = true;
    app.register_mouse_button(
        Rect {
            x: 40,
            y: 6,
            width: 9,
            height: 1,
        },
        MouseTarget::RadioSelect(RadioMode::Discovery),
    );
    let cmds = app.update(Msg::MouseClick { col: 43, row: 6 });
    assert_eq!(app.config.radio.mode, RadioMode::Discovery);
    assert!(!app.dropdowns.radio_open);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))));
}

// --- Mouse: nav bar, clickable lists/tabs, and the queue window --------------

/// Render `app` to an 80x24 test terminal so its per-frame mouse hit rects are published
/// (each frame clears and re-registers them, mirroring the real loop).
fn render_app(app: &App) {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
}

fn render_app_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn assert_opaque_rect(buffer: &ratatui::buffer::Buffer, rect: ratatui::layout::Rect) {
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let cell = buffer.cell((x, y)).expect("cell is inside the buffer");
            assert_ne!(
                cell.bg,
                ratatui::style::Color::Reset,
                "popup cell at ({x},{y}) kept the default background"
            );
        }
    }
}

fn assert_rgb_at_least(color: ratatui::style::Color, min: (u8, u8, u8)) {
    let ratatui::style::Color::Rgb(r, g, b) = color else {
        panic!("expected RGB color, got {color:?}");
    };
    assert!(
        r >= min.0 && g >= min.1 && b >= min.2,
        "expected color channels at least {min:?}, got ({r},{g},{b})"
    );
}

fn dropdown_popup_rect(
    app: &App,
    mut is_row: impl FnMut(MouseTarget) -> bool,
) -> ratatui::layout::Rect {
    let rects: Vec<_> = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .filter_map(|b| is_row(b.target).then_some(b.rect))
        .collect();
    assert!(
        !rects.is_empty(),
        "dropdown row rects were not registered; targets: {:?}",
        app.bridges
            .mouse_buttons
            .borrow()
            .iter()
            .map(|b| b.target)
            .collect::<Vec<_>>()
    );

    let left = rects.iter().map(|r| r.left()).min().unwrap();
    let top = rects.iter().map(|r| r.top()).min().unwrap();
    let right = rects.iter().map(|r| r.right()).max().unwrap();
    let bottom = rects.iter().map(|r| r.bottom()).max().unwrap();
    ratatui::layout::Rect::new(
        left.saturating_sub(1),
        top.saturating_sub(1),
        right - left + 2,
        bottom - top + 2,
    )
}

fn centered_percent(area: ratatui::layout::Rect, pct_w: u16, pct_h: u16) -> ratatui::layout::Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

fn centered_fixed(area: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

fn about_icon_rect(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup = centered_fixed(area, 60, 22);
    let inner = ratatui::layout::Rect {
        x: popup.x.saturating_add(1),
        y: popup.y.saturating_add(1),
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let band = ratatui::layout::Rect {
        height: 9.min(inner.height),
        ..inner
    };
    let h = band.height.clamp(1, 9);
    let w = (h * 2).min(band.width);
    ratatui::layout::Rect {
        x: band.x + band.width.saturating_sub(w) / 2,
        y: band.y + band.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The center cell of the hit rect registered for `target` in the last render.
fn button_center(app: &App, target: MouseTarget) -> (u16, u16) {
    app.bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == target)
        .map(|b| (b.rect.x + b.rect.width / 2, b.rect.y + b.rect.height / 2))
        .unwrap_or_else(|| panic!("no hit rect registered for {target:?}"))
}

/// Render `app`, then click the center of `target`'s hit rect.
fn click_target(app: &mut App, target: MouseTarget) -> Vec<Cmd> {
    render_app(app);
    let (col, row) = button_center(app, target);
    app.update(Msg::MouseClick { col, row })
}

fn double_click_target(app: &mut App, target: MouseTarget) -> Vec<Cmd> {
    render_app(app);
    let (col, row) = button_center(app, target);
    app.update(Msg::MouseDoubleClick { col, row })
}

#[test]
fn every_screen_renders_the_nav_bar() {
    for mode in [
        Mode::Player,
        Mode::Search,
        Mode::Library,
        Mode::Settings,
        Mode::Ai,
    ] {
        let mut app = app_playing(1, 0);
        app.navigate_to(mode);
        render_app(&app);
        let buttons = app.bridges.mouse_buttons.borrow();
        for nav in [
            Mode::Player,
            Mode::Search,
            Mode::Library,
            Mode::Settings,
            Mode::Ai,
        ] {
            assert!(
                buttons.iter().any(|b| b.target == MouseTarget::Nav(nav)),
                "screen {mode:?} is missing nav item {nav:?}"
            );
        }
    }
}

#[test]
fn clicking_a_nav_item_switches_screens() {
    let mut app = App::new(100);
    assert_eq!(app.mode, Mode::Player);
    click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert_eq!(app.mode, Mode::Library);
    click_target(&mut app, MouseTarget::Nav(Mode::Search));
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.focus, SearchFocus::Input);
}

#[test]
fn clicking_the_search_button_submits_the_query() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    app.search.input = "lofi beats".to_owned();
    let cmds = click_target(&mut app, MouseTarget::SearchSubmit);
    assert!(app.search.searching);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Search { query, source, .. }]
            if query == "lofi beats" && *source == SearchSource::Youtube
    ));
}

#[test]
fn clicking_the_search_input_focuses_the_query_box() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(2);
    app.search.focus = SearchFocus::Results;
    app.search.select_all = true;
    app.dropdowns.search_source_open = true;

    click_target(&mut app, MouseTarget::SearchInput);

    assert_eq!(app.search.focus, SearchFocus::Input);
    assert!(!app.search.select_all);
    assert!(!app.dropdowns.search_source_open);
}

#[test]
fn clicking_a_library_tab_switches_it() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    click_target(&mut app, MouseTarget::LibraryTab(LibraryTab::Favorites));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
}

#[test]
fn clicking_a_settings_tab_switches_it() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    // SettingsTab::ALL[1] is Playback.
    click_target(&mut app, MouseTarget::SettingsTab(1));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::ALL[1]);
}

#[test]
fn single_click_on_a_result_row_selects_it() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(5);
    click_target(&mut app, MouseTarget::ListRow(2));
    assert_eq!(app.search.selected, 2);
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn double_click_on_a_result_row_plays_it() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(5);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::ListRow(3));
    let cmds = app.update(Msg::MouseDoubleClick { col, row });
    assert_eq!(current(&app), "id3");
    assert!(load_url(&cmds).is_some());
}

#[test]
fn clicking_the_position_label_opens_the_queue_window() {
    let mut app = app_playing(5, 2);
    assert!(!app.queue_popup.open);
    click_target(&mut app, MouseTarget::QueuePos);
    assert!(app.queue_popup.open);
    // It opens focused on the currently playing track.
    assert_eq!(app.queue_popup.cursor, 2);
    assert_eq!(app.queue_popup.anchor, 2);
}

#[test]
fn double_clicking_a_queue_row_jumps_to_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open queue window
    assert!(app.queue_popup.open);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(3));
    let cmds = app.update(Msg::MouseDoubleClick { col, row });
    assert_eq!(app.queue.cursor_pos(), 3);
    assert_eq!(current(&app), "id3");
    assert!(!app.queue_popup.open);
    assert!(load_url(&cmds).is_some());
}

#[test]
fn clicking_a_queue_delete_button_removes_that_track() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    click_target(&mut app, MouseTarget::QueueDel(2));
    assert_eq!(app.queue.len(), 4);
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id2"),
        "the removed track should be gone from the queue"
    );
}

#[test]
fn clicking_outside_the_queue_window_closes_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app); // publishes queue_popup.rect
    // Top-left corner is well outside the centered popup.
    let cmds = app.update(Msg::MouseClick { col: 1, row: 1 });
    assert!(!app.queue_popup.open);
    assert!(cmds.is_empty());
}

#[test]
fn drag_selects_a_range_then_delete_removes_all_of_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open, cursor = anchor = 0
    render_app(&app);
    let (start_col, start_row) = button_center(&app, MouseTarget::QueueRow(0));
    app.update(Msg::MouseClick {
        col: start_col,
        row: start_row,
    });
    // Drag down to row 2: anchor stays at 0, so the selection spans 0..=2.
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));
    app.update(Msg::MouseDrag { col, row });
    assert_eq!(app.queue_popup.anchor, 0);
    assert_eq!(app.queue_popup.cursor, 2);
    // The Delete key removes the whole selected range at once.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.len(), 2);
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id3", "id4"]);
}

#[test]
fn queue_delete_current_range_loads_next_after_removed_range() {
    let mut app = app_playing(5, 1);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 3;

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id4");
    assert!(load_url(&cmds).expect("load of next track").contains("id4"));
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "id4"]);
}

#[test]
fn queue_delete_current_tail_range_stops_when_no_next_exists() {
    let mut app = app_playing(3, 1);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 2;

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id0");
    assert!(load_url(&cmds).is_none());
    assert!(has_stop(&cmds), "mpv should stop the deleted current track");
    assert_eq!(app.prefetch.loaded_video_id, None);
    assert!(app.playback.paused);
}

#[test]
fn queue_delete_current_tail_wraps_under_repeat_all() {
    let mut app = app_playing(3, 2);
    app.queue.repeat = crate::queue::Repeat::All;
    app.update(Msg::Key(key(KeyCode::Char('c'))));

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id0");
    assert!(
        load_url(&cmds)
            .expect("load of wrapped track")
            .contains("id0")
    );
}

#[test]
fn queue_drag_after_release_starts_a_fresh_range() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::QueueRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::QueueRow(2));
    let (c4, r4) = button_center(&app, MouseTarget::QueueRow(4));

    app.update(Msg::MouseClick { col: c0, row: r0 });
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (0, 0));
    app.update(Msg::MouseLeftUp);

    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!(
        (app.queue_popup.cursor, app.queue_popup.anchor),
        (2, 2),
        "a new drag after release must not extend the old row-0 selection"
    );
    app.update(Msg::MouseDrag { col: c4, row: r4 });
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (4, 2));
}

#[test]
fn enter_on_queue_drag_range_starts_at_range_beginning() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 3;

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.queue_popup.open);
    assert_eq!(app.queue.cursor_pos(), 1);
    assert_eq!(current(&app), "id1");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
}
