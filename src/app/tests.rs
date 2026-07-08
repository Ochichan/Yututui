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

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::SHIFT,
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
fn streaming_extend_resumes_playback_when_idle() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = None; // the seed ended before this refill landed
    let cmds = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);
    assert!(
        load_url(&cmds).is_some(),
        "should resume by loading the new track"
    );
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("b"));
}

#[test]
fn streaming_extend_prefetches_next_while_playing() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = Some("a".to_owned()); // still playing the seed
    let cmds = app.extend_queue_from_streaming(vec![Song::remote("b", "B", "y", "2:00")]);
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

#[test]
fn advance_over_all_unplayable_queue_terminates_under_repeat_all() {
    let mut app = App::new(100);
    // Nothing but non-video YouTube refs (channel / playlist ids) — every entry is unplayable.
    app.queue.set(
        vec![
            Song::remote("UCfLdIEPs1tYj4ieEdJnyNyw", "A", "A", ""),
            Song::remote("UCanotherchannelidhere00", "B", "B", ""),
            Song::remote("PL123456789012345", "C", "C", ""),
        ],
        0,
    );
    // Repeat-all makes `peek_next()` always `Some` — the old recursion wrapped forever and
    // overflowed the stack. The bounded loop must return without loading anything playable.
    app.queue.repeat = crate::queue::Repeat::All;
    let cmds = app.advance(true);
    assert!(
        load_url(&cmds).is_none(),
        "no playable track exists; advance must terminate and load nothing"
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
fn restores_radio_session_mode_and_last_station_without_autoplaying() {
    let mut app = App::new(100);
    app.library.record_play(&radio_station("older"));
    app.library.record_play(&radio_station("latest"));

    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "dario");
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert_eq!(app.library_tabs(), &LibraryTab::RADIO_MODE);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "rad:latest");
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
}

#[test]
fn restores_cached_radio_mode_even_without_recent_station() {
    let mut app = App::new(100);

    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert!(app.queue.is_empty());
}

#[test]
fn restores_normal_session_mode_from_song_history() {
    let mut app = App::new(100);
    app.library.record_play(&radio_station("station"));
    app.library.record_play(&songs(1)[0]);

    app.restore_last_session_from_library(false);

    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id0");
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
    app.update(PlayerMsg::TimePos(1.1));
    assert!(app.dirty);
    app.dirty = false;
    app.update(PlayerMsg::TimePos(1.9)); // same whole second
    assert!(!app.dirty);
    app.update(PlayerMsg::TimePos(2.0)); // new second
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
                ..
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
        [Cmd::Persist(PersistCmd::Config(cfg))] if cfg.search.source == SearchSource::SoundCloud
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
fn settings_global_key_capture_rejects_player_overlap() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // -> Hotkeys tab
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);

    let row = crate::keymap::editable_entries()
        .iter()
        .position(|entry| *entry == (KeyContext::Global, Action::ToggleHelp))
        .expect("global help binding is editable");
    app.settings.as_mut().unwrap().row = row;
    app.update(Msg::Key(key(KeyCode::Enter))); // capture global.toggle_help
    assert_eq!(
        app.settings.as_ref().unwrap().capturing,
        Some((KeyContext::Global, Action::ToggleHelp))
    );

    app.update(Msg::Key(key(KeyCode::Char('.')))); // Player next-track owns `.`.
    let conflict = app
        .overlays
        .key_conflict
        .expect("global overlap should raise a conflict warning");
    assert_eq!(conflict.ctx, KeyContext::Player);
    assert_eq!(conflict.existing, Action::NextTrack);
    assert_eq!(conflict.chord, crate::keymap::parse_chord(".").unwrap());
}

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
    app.update(Msg::Media(crate::media::MediaCommand::SetVolume(0.5)));
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
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains("abc123"));
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
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
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
fn enqueue_defaults_to_appending_at_the_queue_end() {
    let mut app = app_playing(3, 0);
    app.library
        .record_play(&Song::remote("lib9", "Lib Nine", "Z", "3:00"));
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;
    app.library_ui.selected = 0;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));

    assert!(load_url(&cmds).is_none());
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

    assert!(load_url(&cmds).is_none());
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

fn app_with_three_favorites() -> App {
    let mut app = app_playing(2, 0);
    app.library.favorites = vec![
        Song::remote("f0", "F0", "A", "3:00"),
        Song::remote("f1", "F1", "B", "3:00"),
        Song::remote("f2", "F2", "C", "3:00"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.selected = 0;
    app.library_ui.anchor = 0;
    app
}

#[test]
fn shift_down_extends_library_selection_without_collapsing_anchor() {
    let mut app = app_with_three_favorites();

    // Shift+Down twice grows the range from the anchor at row 0 to the cursor at row 2.
    app.update(Msg::Key(shift(KeyCode::Down)));
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!(app.library_ui.anchor, 0, "anchor stays put while extending");
    assert_eq!(app.library_ui.selected, 2, "cursor advanced");

    // The whole span feeds the existing bulk consumers.
    let selected = app.selected_library_songs();
    let ids: Vec<&str> = selected.iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, ["f0", "f1", "f2"]);
}

#[test]
fn plain_down_collapses_library_selection_onto_the_cursor() {
    let mut app = app_with_three_favorites();
    // Build a range with Shift, then a plain Down collapses it (anchor follows the cursor).
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!((app.library_ui.anchor, app.library_ui.selected), (0, 1));
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.library_ui.selected, 2);
    assert_eq!(app.library_ui.anchor, 2, "plain nav collapses the range");
}

#[test]
fn shift_home_and_end_extend_library_selection_to_the_edges() {
    let mut app = app_with_three_favorites();
    app.library_ui.selected = 1;
    app.library_ui.anchor = 1;

    app.update(Msg::Key(shift(KeyCode::End)));
    assert_eq!(app.library_ui.anchor, 1);
    assert_eq!(
        app.library_ui.selected, 2,
        "Shift+End extends to the bottom"
    );

    app.library_ui.selected = 1;
    app.library_ui.anchor = 1;
    app.update(Msg::Key(shift(KeyCode::Home)));
    assert_eq!(app.library_ui.anchor, 1);
    assert_eq!(app.library_ui.selected, 0, "Shift+Home extends to the top");
}

#[test]
fn shift_down_extends_queue_selection_and_delete_removes_the_range() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open the queue window at the playing row
    assert!(app.queue_popup.open);
    assert_eq!((app.queue_popup.anchor, app.queue_popup.cursor), (0, 0));

    app.update(Msg::Key(shift(KeyCode::Down)));
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!(app.queue_popup.anchor, 0, "queue anchor stays put");
    assert_eq!(app.queue_popup.cursor, 2, "queue cursor advanced");

    // Delete acts on the inclusive anchor..=cursor span (rows 0..=2), leaving 2 of 5.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.len(), 2);
}

#[test]
fn nav_step_for_hold_ramps_up_with_hold_duration() {
    use std::time::Duration;
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(0)), 1);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(399)), 1);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(400)), 2);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(999)), 2);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(1000)), 4);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(1999)), 4);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(2000)), 8);
    assert_eq!(super::nav_step_for_hold(Duration::from_secs(10)), 8);
}

#[test]
fn nav_repeat_step_accelerates_a_hold_but_resets_on_gap_or_direction_change() {
    use std::time::{Duration, Instant};
    let mut app = App::new(100);
    let t0 = Instant::now();
    // A steady cadence under NAV_REPEAT_GAP keeps one hold alive; cumulative hold time
    // (not per-event spacing) drives the ramp.
    let cadence = Duration::from_millis(150);

    // Fresh press moves one row.
    assert_eq!(app.nav_repeat_step_at(t0, Action::MoveDown), 1);
    let mut now = t0;
    let mut steps = Vec::new();
    for _ in 0..8 {
        now += cadence;
        steps.push(app.nav_repeat_step_at(now, Action::MoveDown));
    }
    // Hold grows 150,300,450,600,750,900,1050,1200 ms → ramps as it crosses 400ms and 1s.
    assert_eq!(steps, vec![1, 1, 2, 2, 2, 2, 4, 4]);

    // A gap longer than NAV_REPEAT_GAP restarts the hold (a deliberate fresh tap).
    let after_gap = now + super::NAV_REPEAT_GAP + Duration::from_millis(1);
    assert_eq!(app.nav_repeat_step_at(after_gap, Action::MoveDown), 1);

    // Switching direction also restarts, even within the window.
    assert_eq!(
        app.nav_repeat_step_at(after_gap + Duration::from_millis(10), Action::MoveDown),
        1
    );
    assert_eq!(
        app.nav_repeat_step_at(after_gap + Duration::from_millis(20), Action::MoveUp),
        1,
        "direction change resets the streak"
    );
}

#[test]
fn right_click_on_a_search_row_adds_it_to_the_queue() {
    // Something is already playing, so the right-click must not interrupt it.
    let mut app = app_playing(2, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
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
        .hits
        .regions()
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
fn right_click_on_a_library_delete_cell_adds_that_row_to_the_queue() {
    let mut app = app_playing(1, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library.favorites = vec![Song::remote("fav0", "Favorite Zero", "A", "3:00")];

    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::LibraryDel(0));
    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert_eq!(app.library_ui.selected, 0);
    assert!(load_url(&cmds).is_none());
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert!(app.queue.video_ids().any(|v| v == "fav0"));
    assert!(
        app.library.favorites.iter().any(|s| s.video_id == "fav0"),
        "right-clicking the library delete cell must enqueue, not delete"
    );
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
    app.overlays.help_visible = true;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.overlays.help_visible);
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

fn local_song(stem: &str) -> Song {
    Song::local_file(PathBuf::from(format!("/tmp/{stem}.m4a")))
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

// ---- Radio recorder state machine --------------------------------------------------------

/// A radio station playing with recording enabled in `mode` and a scratch temp dir. The
/// reducer only builds `Cmd`s (no disk IO), so the temp dir is never actually written.
fn recording_app(mode: crate::recorder::RecordingMode) -> App {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());
    app.recorder.supported = true;
    app.recorder.temp_dir = std::path::PathBuf::from("/tmp/ytt-rec-test");
    app.config.recording.mode = mode;
    app
}

fn feed_title(app: &mut App, title: &str) -> Vec<Cmd> {
    app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": title }),
    ))
}

/// Pretend the open segment started `secs` ago so the min/max filters see real duration.
fn backdate_current(app: &mut App, secs: u64) {
    if let Some(seg) = app.recorder.current.as_mut() {
        seg.started_at = seg
            .started_at
            .checked_sub(std::time::Duration::from_secs(secs))
            .unwrap_or(seg.started_at);
    }
}

fn emits_stream_record_clear(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|c| {
        matches!(c, Cmd::Player(crate::player::PlayerCmd::SetProperty { name, value })
            if name == "stream-record" && value == &serde_json::Value::from(""))
    })
}

#[test]
fn recorder_first_track_is_incomplete_and_dropped() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "Artist A - Track 1");
    let seg = app.recorder.current.as_ref().expect("segment open");
    assert!(
        seg.incomplete,
        "the first track after tuning in is incomplete"
    );
    assert!(app.recorder.saw_first_title);
    assert!(app.recorder.history.is_empty());

    // Second title: the incomplete first track is dropped; a complete second opens.
    feed_title(&mut app, "Artist B - Track 2");
    assert!(
        app.recorder.history.is_empty(),
        "incomplete first stays out of history"
    );
    let seg = app.recorder.current.as_ref().expect("second segment open");
    assert!(!seg.incomplete);
    assert_eq!(seg.raw, "Artist B - Track 2");
}

#[test]
fn recorder_keeps_a_long_enough_track_on_the_next_boundary() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete first
    feed_title(&mut app, "B - Two"); // opens complete "Two"
    backdate_current(&mut app, 60); // pretend "Two" played a full minute
    feed_title(&mut app, "C - Three"); // finalize "Two" -> kept
    assert_eq!(app.recorder.history.len(), 1);
    let t = &app.recorder.history[0];
    assert_eq!(t.raw, "B - Two");
    assert_eq!(t.title.as_deref(), Some("Two"));
    assert_eq!(t.artist.as_deref(), Some("B"));
    assert!(matches!(t.state, crate::recorder::RecordingState::Recorded));
}

#[test]
fn recorder_drops_a_track_below_min_duration() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two"); // complete, but ~0s long
    feed_title(&mut app, "C - Three"); // finalize "Two": below min -> dropped
    assert!(app.recorder.history.is_empty());
}

#[test]
fn recorder_everything_mode_auto_saves_kept_tracks() {
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let cmds = feed_title(&mut app, "C - Three");
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::Recorder(crate::recorder::job::RecorderJob::Save { .. })
        )),
        "a save job is emitted for the kept track"
    );
    assert_eq!(app.recorder.history.len(), 1);
    assert!(matches!(
        app.recorder.history[0].state,
        crate::recorder::RecordingState::Saved
    ));
}

#[test]
fn recorder_pauses_through_ad_and_resumes_complete() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete
    feed_title(&mut app, "B - Two"); // complete "Two"
    backdate_current(&mut app, 60);
    // Metadata goes junk (ad / station id) -> parsed None -> finalize "Two", open nothing.
    app.update(PlayerMsg::Metadata(serde_json::json!({ "icy-title": "" })));
    assert!(
        app.recorder.current.is_none(),
        "recording paused during the ad"
    );
    assert_eq!(app.recorder.history.len(), 1, "\"Two\" kept");
    // A later real title opens a *complete* segment (saw_first_title is already set).
    feed_title(&mut app, "D - Four");
    assert!(!app.recorder.current.as_ref().expect("resumed").incomplete);
}

#[test]
fn recorder_teardown_on_stop_drops_in_progress_and_clears_stream_record() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    assert!(app.recorder.current.is_some());
    let cmds = app.load_song(None); // stop / clear playback
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    assert!(emits_stream_record_clear(&cmds));
}

#[test]
fn media_stop_clears_radio_recording_and_video_pause_latch() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    app.playback.paused = false;
    app.video.paused_audio = true;

    let cmds = app.update(Msg::Media(crate::media::MediaCommand::Stop));

    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(!app.video.paused_audio);
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    let clear_pos = cmds
        .iter()
        .position(|cmd| emits_stream_record_clear(std::slice::from_ref(cmd)))
        .expect("stream-record cleared");
    let stop_pos = cmds
        .iter()
        .position(|cmd| matches!(cmd, Cmd::Player(crate::player::PlayerCmd::Stop)))
        .expect("mpv stop emitted");
    assert!(
        clear_pos < stop_pos,
        "stream-record must clear before mpv stops"
    );
}

#[test]
fn recorder_off_mode_records_nothing() {
    let mut app = recording_app(crate::recorder::RecordingMode::Nothing);
    let cmds = feed_title(&mut app, "A - One");
    assert!(app.recorder.current.is_none());
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Cmd::Player(crate::player::PlayerCmd::SetProperty { name, .. }) if name == "stream-record"
        )),
        "no stream-record command when recording is off"
    );
}

#[test]
fn radio_recording_item_hidden_outside_radio_mode() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = false;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = crate::settings::SettingsTab::Playback;
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .fields()
            .contains(&crate::settings::Field::RadioRecording)
    );
}

#[test]
fn radio_recording_popup_opens_edits_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Row 0 = mode; Right cycles Off -> Decide.
    app.recording_settings_key(key(KeyCode::Right));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_mode,
        crate::recorder::RecordingMode::Decide
    );

    // Last row = "Browse recordings…"; Enter opens the recordings browser.
    for _ in 0..(crate::app::RECORDING_POPUP_ROWS - 1) {
        app.recording_settings_key(key(KeyCode::Down));
    }
    app.recording_settings_key(key(KeyCode::Enter));
    assert!(
        app.overlays.recordings_browser.is_some(),
        "browse row opens the browser"
    );

    // Closing Settings commits the draft to config.
    app.overlays.recordings_browser = None;
    app.overlays.recording_settings = None;
    let _ = app.close_settings();
    assert_eq!(
        app.config.recording.mode,
        crate::recorder::RecordingMode::Decide
    );
}

#[test]
fn recording_slider_drag_maps_column_to_value() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(app.overlays.recording_settings.is_some());

    // An 11-cell track; the mapping uses width-1 divisions so both ends are reachable.
    let track = Rect {
        x: 20,
        y: 5,
        width: 11,
        height: 1,
    };

    // Keep-recent (row 4, 1..=50 step 1): leftmost cell → min, rightmost cell → max.
    app.recording_slider_set(4, track.x, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        1
    );
    app.recording_slider_set(4, track.x + 10, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        50
    );

    // Min duration (row 1, 5..=600 step 5): a column past the right end clamps to the max,
    // and one before the left end clamps to the min — proof the drag works both directions.
    app.recording_slider_set(1, track.right() + 5, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        600
    );
    app.recording_slider_set(1, 0, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        5
    );
}

#[test]
fn recorder_saved_emits_desktop_notify_when_enabled() {
    let _guard = crate::i18n::lock_for_test();
    use crate::recorder::job::RecorderEvent;
    let mut app = App::new(100);

    // notify on → a saved recording returns a DesktopNotify command (the filename is the body),
    // in addition to the in-app toast.
    app.config.recording.notify = true;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 1,
        final_path: std::path::PathBuf::from("/tmp/Artist - Track.mp3"),
    });
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::DesktopNotify { body, .. } if body == "Artist - Track.mp3"
        )),
        "a saved recording fires a desktop notification with the filename as the body"
    );

    // notify off → no desktop notification.
    app.config.recording.notify = false;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 2,
        final_path: std::path::PathBuf::from("/tmp/Other.mp3"),
    });
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::DesktopNotify { .. })),
        "with notifications off, no desktop notification is fired"
    );
}

#[test]
fn recording_settings_popup_closed_by_navigation() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Navigating Home must drop the top-level overlay so it can't strand over the Player
    // (regression: it used to keep painting on top, unreachable).
    let _ = app.go_home();
    assert!(
        app.overlays.recording_settings.is_none(),
        "go_home clears the recording-settings popup"
    );
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn radio_stream_metadata_updates_dj_gem_context() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert_eq!(
        app.playback
            .stream_now_playing
            .as_ref()
            .map(StreamNowPlaying::label)
            .as_deref(),
        Some("Track — Artist")
    );
    let ctx = app.build_ai_context();
    assert_eq!(
        ctx.current_radio_station.as_deref(),
        Some("Station groove — KR / MP3")
    );
    assert_eq!(
        ctx.current_radio_now_playing.as_deref(),
        Some("Track — Artist")
    );

    app.dirty = false;
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(!app.dirty, "unchanged stream metadata should not redraw");
}

#[test]
fn stream_metadata_is_ignored_for_regular_tracks() {
    let mut app = app_playing(1, 0);

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert!(app.playback.stream_now_playing.is_none());
    let ctx = app.build_ai_context();
    assert!(ctx.current_radio_station.is_none());
    assert!(ctx.current_radio_now_playing.is_none());
}

#[test]
fn loading_a_new_track_clears_stale_stream_metadata() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(app.playback.stream_now_playing.is_some());

    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());

    assert!(app.playback.stream_now_playing.is_none());
}

// --- Radio live transport: sync verdict + re-sync (M2) -------------------

/// An app playing one live radio station (loaded into mpv).
fn radio_playing(id: &str) -> App {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station(id)], 0);
    app.load_song(app.queue.current().cloned());
    app
}

#[test]
fn cache_time_updates_playback_and_coalesces_redraws() {
    let mut app = radio_playing("groove");
    app.dirty = false;
    app.update(PlayerMsg::CacheTime(Some(100.2)));
    assert_eq!(app.playback.cache_time, Some(100.2));
    assert!(app.playback.cache_time_at.is_some());
    assert!(app.dirty);

    app.dirty = false;
    app.update(PlayerMsg::CacheTime(Some(100.7)));
    assert!(!app.dirty, "same whole second must not redraw");
    app.update(PlayerMsg::CacheTime(Some(101.3)));
    assert!(app.dirty);

    app.dirty = false;
    app.update(PlayerMsg::CacheTime(None));
    assert_eq!(app.playback.cache_time, None);
    assert!(app.playback.cache_time_at.is_none());
    assert!(
        app.dirty,
        "Some→None must redraw so the glyph can't go stale"
    );
}

#[test]
fn radio_live_sync_verdict_follows_behind_distance() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(105.0)));
    assert_eq!(app.radio_behind_secs().map(|b| b as i64), Some(5));
    assert_eq!(app.radio_live_synced(), Some(true));

    app.update(PlayerMsg::CacheTime(Some(180.0)));
    assert_eq!(app.radio_live_synced(), Some(false));

    // A regular track never gets a verdict, even with cache reports flowing.
    let mut music = app_playing(1, 0);
    music.update(PlayerMsg::TimePos(10.0));
    music.update(PlayerMsg::CacheTime(Some(60.0)));
    assert_eq!(music.radio_live_synced(), None);
}

#[test]
fn stale_cache_time_degrades_synced_to_unknown_but_keeps_behind() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(103.0)));
    // An old at-edge report proves nothing while playing (mpv freezes the property
    // once the forward buffer saturates) → unknown.
    app.playback.cache_time_at = Some(Instant::now() - Duration::from_secs(30));
    assert_eq!(app.radio_live_synced(), None);
    // But an old report showing a big gap is still a valid lower bound → behind.
    app.playback.cache_time = Some(200.0);
    assert_eq!(app.radio_live_synced(), Some(false));
    // While paused the frozen report stays authoritative (behind only grows).
    app.playback.cache_time = Some(103.0);
    app.playback.paused = true;
    assert_eq!(app.radio_live_synced(), Some(true));
}

#[test]
fn radio_repeat_key_resyncs_with_a_seek_to_the_live_edge() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(200.0)));
    app.playback.paused = true;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    // Re-sync resumes playback and seeks just short of the newest demuxed data.
    assert!(!app.playback.paused);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Player(PlayerCmd::CyclePause)))
    );
    let target = cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::SeekAbsolute(s)) => Some(*s),
        _ => None,
    });
    assert_eq!(target, Some(198.0));
    // The queue's repeat mode is untouched and nothing is persisted.
    assert!(save_config(&cmds).is_none());
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.radio_resync_at.is_some());
}

#[test]
fn radio_repeat_key_reconnects_when_no_cache_info() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert!(
        load_url(&cmds)
            .expect("stream reconnect")
            .contains("groove")
    );
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
}

#[test]
fn radio_resync_escalates_to_reconnect_when_seek_does_not_take() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(200.0)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(load_url(&cmds).is_none(), "first re-sync seeks");

    // Seconds later the playhead hasn't moved (an unseekable live cache) — the retry
    // inside the window escalates to a stream reconnect.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(
        load_url(&cmds)
            .expect("escalated reconnect")
            .contains("groove")
    );
}

#[test]
fn radio_shift_s_reports_sync_state_without_touching_shuffle() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(103.0)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('S'))));

    assert!(cmds.is_empty(), "the sync note is read-only");
    assert!(!app.queue.shuffle);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(!app.status.text.is_empty());
}

#[test]
fn loading_a_new_track_clears_cache_time() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::CacheTime(Some(50.0)));
    assert!(app.playback.cache_time.is_some());

    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());

    assert!(app.playback.cache_time.is_none());
    assert!(app.playback.cache_time_at.is_none());
    assert!(app.radio_resync_at.is_none());
}

// --- "What's playing" (지듣노) card — ICY metadata, no AI to open (M3) -----

/// A radio app with an ICY title playing. DJ Gem is OFF unless a test enables it.
fn radio_with_title(title: &str) -> App {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": title }),
    ));
    app
}

/// A radio app with the card already open on a playing song (DJ Gem OFF).
fn radio_card(title: &str) -> App {
    let mut app = radio_with_title(title);
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app
}

#[test]
fn identify_key_needs_a_radio_stream() {
    // Music mode: an info note, no overlay.
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_none());
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn identify_opens_from_icy_metadata_without_dj_gem_or_an_api_call() {
    // DJ Gem OFF: the card still opens and shows the stream's song, synchronously.
    let mut app = radio_with_title("Artist - Track");
    assert!(!app.ai.available);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(
        cmds.is_empty(),
        "populated from ICY metadata — never an API call"
    );
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { artist, title })
            if artist.as_deref() == Some("Artist") && title == "Track"
    ));
    // The `i` key still toggles the card closed.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_none());
}

#[test]
fn identify_without_metadata_shows_no_metadata() {
    let mut app = radio_playing("groove");
    let cmds = app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(cmds.is_empty());
    let overlay = app
        .overlays
        .now_playing_overlay
        .as_ref()
        .expect("overlay opens");
    assert_eq!(overlay.state, NowPlayingOverlayState::NoMetadata);
}

#[test]
fn identify_flags_obvious_station_content() {
    let mut app = radio_with_title("Werbung");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(&NowPlayingOverlayState::StationContent)
    );
    // Station content is neither favoritable nor askable.
    assert!(!app.now_playing_can_favorite());
    app.ai.available = true;
    assert!(!app.now_playing_can_ask());
}

#[test]
fn open_card_repopulates_live_on_title_change() {
    let mut app = radio_card("Artist - Track");
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Track"
    ));
    // The song flips under the open card → it re-populates from the fresh ICY title.
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Other - Song"
    })));
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Song"
    ));
}

#[test]
fn early_open_before_first_tick_fills_in_when_metadata_arrives() {
    // Opened right after tuning in, before mpv surfaces the first ICY tick.
    let mut app = radio_playing("groove");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(&NowPlayingOverlayState::NoMetadata)
    );
    // The first metadata tick lands → the open card fills in on its own.
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(matches!(
        app.overlays.now_playing_overlay.as_ref().map(|o| &o.state),
        Some(NowPlayingOverlayState::Playing { title, .. }) if title == "Track"
    ));
}

fn resolve_track_cmd(cmds: &[Cmd]) -> Option<(u64, &str)> {
    cmds.iter().find_map(|c| match c {
        Cmd::ResolveTrack { seq, query, .. } => Some((*seq, query.as_str())),
        _ => None,
    })
}

fn ask_ai_prompt(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::AskAi { prompt, .. } => Some(prompt.as_str()),
        _ => None,
    })
}

#[test]
fn overlay_favorite_resolves_then_adds_to_music_favorites_once() {
    // Favoriting is AI-free: DJ Gem stays OFF here.
    let mut app = radio_card("Artist - Track");

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq, query) = resolve_track_cmd(&cmds).expect("one resolve");
    assert_eq!(query, "Artist Track");
    assert!(
        app.overlays
            .now_playing_overlay
            .as_ref()
            .is_some_and(|o| o.resolving)
    );

    // A second press while resolving is a no-op (debounced).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(resolve_track_cmd(&cmds).is_none());

    let cmds = app.update(Msg::TrackResolved {
        seq,
        result: Ok(vec![
            Song::remote("vid1", "Track", "Artist", "3:00"),
            Song::remote("vid2", "Track (Live)", "Artist", "4:00"),
        ]),
    });
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        app.library.is_favorite("vid1"),
        "best match lands in favorites"
    );
    assert!(
        app.library.favorites.iter().any(|s| s.video_id == "vid1"),
        "a Youtube-source track routes to the MUSIC favorites"
    );
    assert!(
        app.library
            .radio_favorites
            .iter()
            .all(|s| s.video_id != "vid1")
    );
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Track"));

    // Repeat press: the resolved song rides the overlay — no re-search, and the
    // toggle-precheck must NOT remove the favorite.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(resolve_track_cmd(&cmds).is_none());
    assert!(app.library.is_favorite("vid1"));

    // Re-open the card for the same title: the cache carries the resolution too.
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(
        resolve_track_cmd(&cmds).is_none(),
        "cache-attached resolution reused"
    );
    assert!(app.library.is_favorite("vid1"));
}

#[test]
fn overlay_favorite_resolve_failures_keep_the_overlay_and_write_nothing() {
    let mut app = radio_card("Artist - Track");
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq, _) = resolve_track_cmd(&cmds).expect("resolve");

    // Empty result: an error toast, the overlay stays, nothing is written.
    let cmds = app.update(Msg::TrackResolved {
        seq,
        result: Ok(Vec::new()),
    });
    assert!(cmds.is_empty());
    assert!(app.overlays.now_playing_overlay.is_some());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.library.favorites.is_empty());

    // Stale seq: dropped entirely.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    let (seq2, _) = resolve_track_cmd(&cmds).expect("second resolve");
    app.update(Msg::TrackResolved {
        seq: seq2.wrapping_sub(1),
        result: Ok(vec![Song::remote("vid9", "Wrong", "X", "1:00")]),
    });
    assert!(
        app.library.favorites.is_empty(),
        "stale reply must not write"
    );
    assert!(
        app.overlays
            .now_playing_overlay
            .as_ref()
            .is_some_and(|o| o.resolving)
    );
}

#[test]
fn overlay_ask_ai_is_gated_on_dj_gem_and_seeds_a_rich_block() {
    // DJ Gem OFF: the ask action is hidden and pressing its key does nothing.
    let mut app = radio_card("Artist - Track");
    assert!(!app.now_playing_can_ask());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert!(ask_ai_prompt(&cmds).is_none());
    assert!(
        app.overlays.now_playing_overlay.is_some(),
        "the card stays put"
    );

    // DJ Gem ON: the ask action hands off with a labeled, enriched block.
    app.ai.available = true;
    assert!(app.now_playing_can_ask());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    let prompt = ask_ai_prompt(&cmds).expect("one AskAi");
    assert_eq!(app.mode, Mode::Ai);
    assert!(
        app.overlays.now_playing_overlay.is_none(),
        "the card closes on handoff"
    );
    assert!(app.ai.thinking);
    // The model gets the labeled block: station, untrusted raw title, local split, a
    // standing "may be mislabeled" caution, and the rich structured request.
    assert!(prompt.starts_with("<now_playing>"));
    assert!(prompt.contains("station: Station groove — KR / MP3"));
    assert!(prompt.contains("raw_title (untrusted, as sent by the radio stream): Artist - Track"));
    assert!(prompt.contains("parsed (best-effort local split): artist=Artist · title=Track"));
    assert!(prompt.contains("may be mislabeled"));
    assert!(prompt.contains("similar tracks"));
    assert!(prompt.contains("</now_playing>"));
    // The transcript shows the compact line, not the block.
    let last = app.ai.messages.last().expect("transcript line");
    assert_eq!(last.role, AiRole::User);
    assert!(!last.text.contains("<now_playing>"));
    assert!(last.text.contains("Track — Artist"));
}

#[test]
fn overlay_ask_ai_respects_the_thinking_guard() {
    // DJ Gem connected but busy → an info note; the card stays put and nothing is sent.
    let mut app = radio_card("Artist - Track");
    app.ai.available = true;
    app.ai.thinking = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert!(ask_ai_prompt(&cmds).is_none());
    assert!(app.overlays.now_playing_overlay.is_some());
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn identify_overlay_swallows_player_keys_and_esc_closes() {
    let mut app = radio_with_title("Artist - Track");
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert!(app.overlays.now_playing_overlay.is_some());

    // `n` (next track) must not leak through to the player underneath.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(cmds.is_empty());
    assert_eq!(current(&app), "rad:groove");
    assert!(app.overlays.now_playing_overlay.is_some());

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.overlays.now_playing_overlay.is_none());
}

#[test]
fn eof_auto_advances_to_next_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Eof);
    assert!(load_url(&cmds).expect("load of next track").contains("id1"));
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_at_end_with_repeat_off_stops() {
    let mut app = app_playing(2, 1); // already on the last track
    let cmds = app.update(PlayerMsg::Eof);
    // Playback stops (no load/advance), though the finished track is still recorded.
    assert!(load_url(&cmds).is_none());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_with_repeat_one_replays_same_track() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    let cmds = app.update(PlayerMsg::Eof);
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
    let cmds = app.update(PlayerMsg::Error("boom".to_owned()));
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
        let cmds = app.update(PlayerMsg::Error("boom".to_owned()));
        assert!(load_url(&cmds).is_some(), "still skipping within budget");
    }
    // ...the next one gives up instead of skip-storming the whole queue.
    let cmds = app.update(PlayerMsg::Error("boom".to_owned()));
    assert!(load_url(&cmds).is_none(), "stops skipping after the budget");
    assert!(app.status.text.contains("stopped") || app.status.text.contains("failed"));
}

#[test]
fn successful_playback_resets_the_error_streak() {
    let mut app = app_playing(5, 0);
    app.update(PlayerMsg::Error("boom".to_owned())); // skip to id1 (streak = 1)
    assert_eq!(current(&app), "id1");
    app.update(PlayerMsg::TimePos(3.0)); // id1 actually plays → streak cleared
    // A later failure starts a fresh streak, so it skips again rather than giving up.
    let cmds = app.update(PlayerMsg::Error("boom".to_owned()));
    assert!(
        load_url(&cmds)
            .expect("skips again after a clean play")
            .contains("id2")
    );
    assert_eq!(current(&app), "id2");
}

/// The mpv `file_error` signature of a failed ytdl_hook resolution (stale yt-dlp),
/// exactly as `player::ipc` wraps it.
const EXTRACTION_ERR: &str = "mpv could not play this track (unrecognized file format)";

fn heal_cmd_id(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::YtdlpSelfHeal { video_id, .. } => Some(video_id.as_str()),
        _ => None,
    })
}

fn resolve_cmd_id(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Resolve { video_id, .. } => Some(video_id.as_str()),
        _ => None,
    })
}

#[test]
fn extraction_error_triggers_ytdlp_self_heal_instead_of_skipping() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    assert_eq!(
        heal_cmd_id(&cmds),
        Some("id0"),
        "runs a yt-dlp update check"
    );
    assert!(load_url(&cmds).is_none(), "no skip while the heal runs");
    assert_eq!(current(&app), "id0", "cursor stays on the failed track");
    assert!(app.status.text.contains("yt-dlp"));
}

#[test]
fn heal_success_resolves_and_reloads_the_same_track() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    // A new binary landed → the track re-resolves through the resolver (the session
    // mpv keeps its stale spawn-time ytdl_hook, so a watch-URL reload wouldn't help).
    let cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    assert_eq!(resolve_cmd_id(&cmds), Some("id0"));
    assert!(load_url(&cmds).is_none(), "load waits for the resolve");
    // The fresh binary's direct CDN URL arrives → the SAME track reloads from it.
    let cmds = app.update(StreamingMsg::Resolved {
        video_id: "id0".to_owned(),
        stream_url: "https://cdn.example/id0".to_owned(),
    });
    assert_eq!(load_url(&cmds), Some("https://cdn.example/id0"));
    assert_eq!(current(&app), "id0");
}

#[test]
fn heal_without_update_falls_back_to_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    let cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: false,
    });
    assert!(load_url(&cmds).expect("skips to next").contains("id1"));
    assert_eq!(current(&app), "id1");
    assert!(
        app.status.text.contains("yt-dlp"),
        "message names the cause"
    );
}

#[test]
fn heal_resolve_failure_falls_back_to_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    // The updated binary STILL can't resolve it (region lock, deleted video…).
    let cmds = app.update(Msg::ResolveFailed {
        video_id: "id0".to_owned(),
    });
    assert!(load_url(&cmds).expect("skips to next").contains("id1"));
    assert_eq!(current(&app), "id1");
}

#[test]
fn heal_runs_once_per_track_then_plain_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: false,
    });
    assert_eq!(current(&app), "id1");
    // Back on the same track later: no second heal (and the 30-min cooldown also
    // bars other tracks from re-checking) — the plain skip path runs instead.
    app.update(Msg::Key(key(KeyCode::Char(','))));
    assert_eq!(current(&app), "id0");
    let cmds = app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    assert!(
        heal_cmd_id(&cmds).is_none(),
        "one heal per track per session"
    );
    assert!(load_url(&cmds).expect("plain skip").contains("id1"));
}

#[test]
fn stale_heal_result_is_ignored_after_user_moved_on() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    // The user skips manually while the update check is still running.
    app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert_eq!(current(&app), "id1");
    let cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    assert!(cmds.is_empty(), "stale heal result is dropped");
    assert_eq!(current(&app), "id1");
}

#[test]
fn non_extraction_error_never_triggers_a_heal() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    assert!(
        heal_cmd_id(&cmds).is_none(),
        "network errors skip as before"
    );
    assert!(load_url(&cmds).expect("plain skip").contains("id1"));
}

#[test]
fn next_and_prev_keys_move_through_queue() {
    let mut app = app_playing(3, 0);
    app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert_eq!(current(&app), "id1");
    app.update(Msg::Key(key(KeyCode::Char(','))));
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
    let cmds = app.update(Msg::Key(key(KeyCode::Char(']'))));
    assert!((app.playback.speed - 1.1).abs() < 1e-9);
    assert!(cmds.iter().any(|c| matches!(c,
        Cmd::Player(PlayerCmd::SetProperty { name, .. }) if name == "speed")));
    // Floor at SPEED_MIN no matter how many times we press down.
    for _ in 0..50 {
        app.update(Msg::Key(key(KeyCode::Char('['))));
    }
    assert!((app.playback.speed - SPEED_MIN).abs() < 1e-9);
}

#[test]
fn ctrl_r_toggles_autoplay_streaming() {
    let mut app = app_playing(3, 0);
    assert!(!app.autoplay_streaming);
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming);
    assert_eq!(
        save_config(&cmds)
            .expect("a SaveConfig cmd")
            .autoplay_streaming,
        Some(true)
    );
    // Plain `r` while autoplay is on is now refused (they're mutually exclusive in music mode):
    // repeat stays Off, a status message is shown, and autoplay is untouched.
    app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert!(!app.status.text.is_empty());
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_streaming);
    assert_eq!(
        save_config(&cmds)
            .expect("a SaveConfig cmd")
            .autoplay_streaming,
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
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
    assert!(!app.radio_dedicated_mode);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.radio_dedicated_mode);
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
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
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Exit)
    );
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
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Library;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Player;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

fn local_deck_track(
    path: &str,
    title: &str,
    artist: &[&str],
    album: Option<&str>,
    album_artist: Option<&str>,
    genre: &[&str],
    modified_at: i64,
) -> crate::local::LocalTrack {
    let mut track = crate::local::LocalTrack::untagged(
        PathBuf::from(path),
        path.len() as u64 + 100,
        modified_at,
    );
    track.title = title.to_owned();
    track.artist = artist.iter().map(|value| (*value).to_owned()).collect();
    track.album = album.map(str::to_owned);
    track.album_artist = album_artist.map(str::to_owned);
    track.genre = genre.iter().map(|value| (*value).to_owned()).collect();
    track.duration_ms = Some(60_000);
    track
}

fn app_with_local_deck_index(tracks: Vec<crate::local::LocalTrack>) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(tracks);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            summary: crate::local::LocalScanSummary {
                indexed: index.tracks().len(),
                added: index.tracks().len(),
                ..crate::local::LocalScanSummary::default()
            },
            index,
            errors: Vec::new(),
        },
    }));
    app
}

#[test]
fn double_click_library_nav_confirms_local_deck_shell() {
    let mut app = App::new(100);
    app.mode = Mode::Library;

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
    assert!(!app.local_dedicated_mode);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert!(app.local_mode.pending_confirm.is_none());

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.local_dedicated_mode);
    assert!(app.local_mode.pending_confirm.is_none());
}

#[test]
fn local_deck_and_radio_mode_are_mutually_exclusive() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    assert!(app.local_dedicated_mode);

    app.mode = Mode::Player;
    let cmds = app.request_radio_mode_switch();
    assert!(cmds.is_empty());
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);
    assert!(!app.status.text.is_empty());

    app.apply_local_mode_confirm(LocalModeConfirm::Exit);
    assert!(!app.local_dedicated_mode);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert!(app.radio_dedicated_mode);

    app.mode = Mode::Library;
    let cmds = app.request_local_mode_switch();
    assert!(cmds.is_empty());
    assert!(app.local_mode.pending_confirm.is_none());
    assert!(app.radio_dedicated_mode);
}

#[test]
fn alt_shift_l_confirms_local_deck_enter_and_exit_from_keyboard() {
    let mut app = App::new(100);
    app.mode = Mode::Library;

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('l'))));

    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.local_dedicated_mode);

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('l'))));

    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.pending_confirm, Some(LocalModeConfirm::Exit));
}

#[test]
fn local_deck_keyboard_toggle_uses_user_rebound_key() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.keymap
        .rebind(
            KeyContext::Library,
            Action::ToggleLocalMode,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();

    app.update(Msg::Key(key(KeyCode::F(8))));

    assert_eq!(
        app.local_mode.pending_confirm,
        Some(LocalModeConfirm::Enter)
    );
}

#[test]
fn local_deck_renders_download_seed_rows_and_activates_them() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/Alpha.m4a"))];
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "LOCAL DECK"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::LocalRow(0))
    );

    let cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!cmds.is_empty());
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Alpha"));
}

#[test]
fn local_deck_enter_loads_index_then_scans_download_root_when_empty() {
    let mut app = App::new(100);
    let root = PathBuf::from("/tmp/yututui-local-deck-test-root");
    app.config.download_dir = Some(root.clone());

    let enter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert!(app.local_dedicated_mode);
    assert!(app.local_mode.index.loading);
    assert!(
        enter
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::LoadIndex { .. })))
    );

    let scan = app.update(Msg::Local(LocalMsg::IndexLoaded {
        index_path: None,
        index: crate::local::LocalIndex::default(),
    }));

    assert!(app.local_mode.index.scanning);
    let Some(Cmd::Local(LocalCmd::ScanRoots {
        roots, previous, ..
    })) = scan
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck scan command after empty index load");
    };
    assert_eq!(roots, &vec![crate::local::LocalScanRoot::download(root)]);
    assert!(previous.is_empty());
}

#[test]
fn local_deck_scan_roots_follow_local_config() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/yututui-local-downloads");
    let music = PathBuf::from("/tmp/yututui-music-root");
    app.config.download_dir = Some(downloads.clone());
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: music.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];

    assert_eq!(
        app.local_scan_roots(),
        vec![
            crate::local::LocalScanRoot::download(downloads.clone()),
            crate::local::LocalScanRoot::recursive(music.clone()),
        ]
    );

    app.config.local.include_download_dir = Some(false);
    assert_eq!(
        app.local_scan_roots(),
        vec![crate::local::LocalScanRoot::recursive(music)]
    );
}

#[test]
fn local_deck_scan_roots_merge_duplicate_download_root_recursively() {
    let mut app = App::new(100);
    let root = PathBuf::from("/tmp/yututui-local-merged-root");
    app.config.download_dir = Some(root.clone());
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: root.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];

    assert_eq!(
        app.local_scan_roots(),
        vec![crate::local::LocalScanRoot::recursive(root)]
    );
}

#[test]
fn local_deck_scan_result_replaces_seed_rows_and_activates_index_track() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/Seed.m4a"))];
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    let mut track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Indexed.flac"), 7, 8);
    track.title = "Indexed Title".to_owned();
    track.artist = vec!["Indexed Artist".to_owned()];
    track.duration_ms = Some(61_000);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index,
            summary: crate::local::LocalScanSummary {
                indexed: 1,
                added: 1,
                ..crate::local::LocalScanSummary::default()
            },
            errors: Vec::new(),
        },
    }));

    assert_eq!(app.local_rows_len(), 1);
    assert_eq!(app.local_mode.ui.section, LocalSection::Tracks);

    let cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));

    assert!(!cmds.is_empty());
    assert_eq!(
        app.queue.current().map(|s| s.title.as_str()),
        Some("Indexed Title")
    );
    assert_eq!(
        app.queue.current().map(|s| s.artist.as_str()),
        Some("Indexed Artist")
    );
}

#[test]
fn local_deck_r_key_requests_incremental_rescan() {
    let mut app = App::new(100);
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    app.local_mode.index.loading = false;
    app.local_mode.index.loaded = true;
    let track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Indexed.flac"), 7, 8);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.local_mode.index.index = index;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert!(app.local_mode.index.scanning);
    let Some(Cmd::Local(LocalCmd::ScanRoots { previous, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan command");
    };
    assert_eq!(previous.tracks().len(), 1);
}

#[test]
fn local_deck_scan_progress_updates_status_line_until_finished() {
    let mut app = app_with_local_deck_index(Vec::new());
    app.config.download_dir = Some(PathBuf::from("/tmp/music"));

    app.request_local_scan(false);
    app.update(Msg::Local(LocalMsg::ScanProgress(
        crate::local::LocalScanProgress {
            seen: 3,
            indexed: 2,
            skipped: 1,
            errors: 1,
            current: Some(PathBuf::from("/tmp/music/song.flac")),
        },
    )));

    assert!(app.local_mode.index.scanning);
    assert_eq!(
        app.local_mode
            .index
            .progress
            .as_ref()
            .map(|progress| progress.seen),
        Some(3)
    );
    assert!(app.status.text.contains("3 seen"));
    assert!(app.status.text.contains("2 indexed"));
    assert!(app.status.text.contains("song.flac"));
    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "3 seen"));
    assert!(buffer_contains(&buf, "2 indexed"));

    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index: crate::local::LocalIndex::default(),
            summary: crate::local::LocalScanSummary::default(),
            errors: Vec::new(),
        },
    }));

    assert!(app.local_mode.index.progress.is_none());
    assert!(!app.local_mode.index.scanning);
}

#[test]
fn local_deck_slash_filters_index_tracks_and_activation_uses_visible_row() {
    let mut app = App::new(100);
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    let mut alpha = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Alpha.flac"), 7, 8);
    alpha.title = "Alpha".to_owned();
    let mut beta = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Beta.flac"), 9, 10);
    beta.title = "Beta".to_owned();
    beta.artist = vec!["Filtered Artist".to_owned()];
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![alpha, beta]);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index,
            summary: crate::local::LocalScanSummary {
                indexed: 2,
                added: 2,
                ..crate::local::LocalScanSummary::default()
            },
            errors: Vec::new(),
        },
    }));

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.local_mode.ui.filter_editing);
    for ch in "filtered".chars() {
        app.update(Msg::Key(key(KeyCode::Char(ch))));
    }

    assert_eq!(app.local_rows_len(), 1);
    let cmds = double_click_target(&mut app, MouseTarget::LocalRow(0));

    assert!(!cmds.is_empty());
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
}

#[test]
fn local_deck_escape_clears_committed_filter_before_exit() {
    let mut app = App::new(100);
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    let mut track = crate::local::LocalTrack::untagged(PathBuf::from("/tmp/Alpha.flac"), 7, 8);
    track.title = "Alpha".to_owned();
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(vec![track]);
    app.local_mode.index.index = index;
    app.local_mode.index.loaded = true;

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.local_mode.ui.filter_editing);
    assert_eq!(app.local_mode.ui.filter_query, "a");

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(app.local_dedicated_mode);
    assert!(app.local_mode.pending_confirm.is_none());
    assert!(app.local_mode.ui.filter_query.is_empty());
}

#[test]
fn local_deck_sidebar_switches_sections_with_mouse_and_number_keys() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/Daft Punk/Discovery/One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    )]);

    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "3 Albums"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::LocalNav(2))
    );

    let cmds = click_target(&mut app, MouseTarget::LocalNav(2));
    assert!(cmds.is_empty());
    assert_eq!(app.local_mode.ui.section, LocalSection::Albums);
    assert_eq!(app.local_rows_len(), 1);

    app.update(Msg::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Artists);
    assert_eq!(app.local_rows_len(), 1);
}

#[test]
fn local_deck_album_rows_drill_down_to_tracks_and_play() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);

    app.update(Msg::Key(key(KeyCode::Char('3'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Albums);
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("Discovery")
    );

    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    assert_eq!(app.local_mode.ui.drill.len(), 1);
    assert_eq!(app.local_rows_len(), 2);

    let play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
}

#[test]
fn local_deck_artist_rows_open_album_drill_down() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/IU/Palette/Palette.flac",
        "Palette",
        &["IU"],
        Some("Palette"),
        Some("IU"),
        &["K-Pop"],
        10,
    )]);

    app.update(Msg::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Artists);
    assert_eq!(app.local_rows_len(), 1);

    let open_artist = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_artist.is_empty());
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("Palette")
    );

    let open_album = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_album.is_empty());
    assert_eq!(app.local_rows_len(), 1);

    let play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("Palette")
    );
}

#[test]
fn local_deck_folder_smart_and_scan_error_sections_render_rows() {
    let untagged = local_deck_track(
        "/tmp/music/Misc/untagged.flac",
        "untagged",
        &[],
        None,
        None,
        &[],
        9,
    );
    let tagged = local_deck_track(
        "/tmp/music/Tagged/song.flac",
        "Song",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Indie"],
        10,
    );
    let mut app = app_with_local_deck_index(vec![untagged, tagged]);
    app.local_mode.index.errors = vec![crate::local::ScanError {
        path: PathBuf::from("/tmp/music/bad.mp3"),
        message: "bad tags".to_owned(),
    }];

    app.update(Msg::Key(key(KeyCode::Char('6'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::Folders);
    assert_eq!(app.local_rows_len(), 2);
    let open_folder = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open_folder.is_empty());
    assert_eq!(app.local_rows_len(), 1);

    app.update(Msg::Key(key(KeyCode::Esc)));
    app.update(Msg::Key(key(KeyCode::Char('7'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::SmartLists);
    let open_missing_artist = double_click_target(&mut app, MouseTarget::LocalRow(3));
    assert!(open_missing_artist.is_empty());
    assert_eq!(app.local_rows_len(), 1);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("untagged")
    );

    app.update(Msg::Key(key(KeyCode::Esc)));
    app.update(Msg::Key(key(KeyCode::Char('8'))));
    assert_eq!(app.local_mode.ui.section, LocalSection::ScanErrors);
    let buf = render_app_buffer(&app, 100, 24);
    assert!(buffer_contains(&buf, "bad tags"));
}

#[test]
fn local_deck_smart_lists_report_counts_for_every_shipped_list() {
    let mut downloaded = local_deck_track(
        "/tmp/music/Ytm/downloaded.m4a",
        "Downloaded",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Pop"],
        12,
    );
    downloaded.linked_video_id = Some("abcdefghijk".to_owned());
    downloaded.embedded_art_key = Some("cover".to_owned());

    let mut missing = local_deck_track(
        "/tmp/music/Misc/missing.mp3",
        "Missing",
        &[],
        None,
        None,
        &[],
        11,
    );
    missing.file_size = 60 * 1024 * 1024;

    let mut lossless = local_deck_track(
        "/tmp/music/Tagged/lossless.flac",
        "Lossless",
        &["Band"],
        Some("Record"),
        Some("Band"),
        &["Rock"],
        10,
    );
    lossless.embedded_art_key = Some("cover".to_owned());

    let mut app = app_with_local_deck_index(vec![downloaded, missing, lossless]);
    app.update(Msg::Key(key(KeyCode::Char('7'))));

    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();

    for expected in [
        "Recently Added  (3 tracks)",
        "Downloaded from YouTube Music  (1 tracks)",
        "Local-only  (2 tracks)",
        "Missing Artist  (1 tracks)",
        "Missing Album  (1 tracks)",
        "No Embedded Cover  (1 tracks)",
        "Large Files  (1 tracks)",
        "Lossless  (1 tracks)",
    ] {
        assert!(
            labels.iter().any(|label| label == expected),
            "missing smart list label {expected:?} in {labels:?}"
        );
    }
}

#[test]
fn local_deck_details_include_selected_track_metadata_and_up_next() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.year = Some(2001);
    first.disc_no = Some(1);
    first.track_no = Some(1);
    first.duration_ms = Some(61_000);
    first.format = Some(crate::local::AudioFormat::Flac);
    first.sample_rate = Some(44_100);
    first.bitrate = Some(320_000);
    first.embedded_art_key = Some("embedded-cover".to_owned());
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![first.clone(), second.clone()]);
    app.config.download_dir = Some(PathBuf::from("/tmp/music"));
    app.queue.set(vec![first.to_song(), second.to_song()], 0);

    let lines = app.local_details_lines();

    for expected in [
        "Title: One More Time",
        "Artist: Daft Punk",
        "Album: Discovery · 2001",
        "Track: disc 1 · track 1",
        "Duration: 1:01",
        "Format: FLAC",
        "Sample rate: 44.1 kHz",
        "Bitrate: 320 kbps",
        "Cover: embedded cover",
        "File: 01 One More Time.flac",
        "Path: Daft Punk/Discovery/01 One More Time.flac",
        "1. Aerodynamic - Daft Punk  (1:00)",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing {expected:?} in {lines:?}"
        );
    }
}

#[test]
fn local_deck_render_expands_details_then_collapses_to_summary() {
    let mut track = local_deck_track(
        "/tmp/music/IU/Palette/Palette.flac",
        "Palette",
        &["IU"],
        Some("Palette"),
        Some("IU"),
        &["K-Pop"],
        10,
    );
    track.year = Some(2017);
    track.embedded_art_key = Some("embedded-cover".to_owned());
    let app = app_with_local_deck_index(vec![track]);

    let wide = render_app_buffer(&app, 120, 30);
    assert!(buffer_contains(&wide, "Selected"));
    assert!(buffer_contains(&wide, "Title: Palette"));
    assert!(buffer_contains(&wide, "Cover: embedded cover"));

    let medium = render_app_buffer(&app, 90, 24);
    assert!(buffer_contains(&medium, "Selected: Palette - IU"));
}

#[test]
fn local_deck_a_enqueues_selected_track_without_interrupting_current() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/local-alpha.flac",
        "Local Alpha",
        &["Local Artist"],
        None,
        None,
        &[],
        10,
    )]);
    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());
    app.mode = Mode::Library;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));

    assert!(load_url(&cmds).is_none());
    assert_eq!(current(&app), "id0");
    assert_eq!(app.queue.len(), 2);
    let ordered: Vec<_> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.title.as_str())
        .collect();
    assert_eq!(ordered, vec!["t0", "Local Alpha"]);
}

#[test]
fn local_deck_shift_a_enqueues_visible_filtered_rows() {
    let mut app = app_with_local_deck_index(vec![
        local_deck_track(
            "/tmp/music/a-alpha.flac",
            "Alpha",
            &["A"],
            None,
            None,
            &[],
            10,
        ),
        local_deck_track(
            "/tmp/music/b-beta.flac",
            "Beta",
            &["Filtered Artist"],
            None,
            None,
            &[],
            11,
        ),
    ]);
    app.local_mode.ui.filter_query = "filtered".to_owned();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('A'))));

    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
    assert!(
        load_url(&cmds)
            .expect("filtered local load")
            .contains("b-beta")
    );
}

#[test]
fn local_deck_p_plays_selected_collection_now() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);
    app.update(Msg::Key(key(KeyCode::Char('3'))));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('P'))));

    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
    assert!(load_url(&cmds).expect("album load").contains("01 One"));
}

#[test]
fn local_deck_s_shuffles_current_view_from_selected_row() {
    let mut app = app_with_local_deck_index(vec![
        local_deck_track(
            "/tmp/music/a-alpha.flac",
            "Alpha",
            &["A"],
            None,
            None,
            &[],
            10,
        ),
        local_deck_track(
            "/tmp/music/b-beta.flac",
            "Beta",
            &["B"],
            None,
            None,
            &[],
            11,
        ),
        local_deck_track(
            "/tmp/music/c-gamma.flac",
            "Gamma",
            &["C"],
            None,
            None,
            &[],
            12,
        ),
    ]);
    app.update(Msg::Key(key(KeyCode::Down)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));

    assert_eq!(app.mode, Mode::Player);
    assert!(app.queue.shuffle);
    assert_eq!(app.queue.len(), 3);
    assert_eq!(app.queue.current().map(|s| s.title.as_str()), Some("Beta"));
    assert!(
        load_url(&cmds)
            .expect("shuffled local load")
            .contains("b-beta")
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn local_deck_c_opens_queue_popup_and_space_toggles_pause() {
    let mut app = app_with_local_deck_index(vec![local_deck_track(
        "/tmp/music/local-alpha.flac",
        "Local Alpha",
        &["Local Artist"],
        None,
        None,
        &[],
        10,
    )]);
    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());
    app.mode = Mode::Library;

    app.update(Msg::Key(key(KeyCode::Char('c'))));
    assert!(app.queue_popup.open);

    app.queue_popup.open = false;
    app.playback.paused = false;
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));

    assert!(app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));
}

#[test]
fn right_clicking_local_deck_collection_enqueues_it() {
    let mut first = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/01 One More Time.flac",
        "One More Time",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        10,
    );
    first.track_no = Some(1);
    let mut second = local_deck_track(
        "/tmp/music/Daft Punk/Discovery/02 Aerodynamic.flac",
        "Aerodynamic",
        &["Daft Punk"],
        Some("Discovery"),
        Some("Daft Punk"),
        &["House"],
        11,
    );
    second.track_no = Some(2);
    let mut app = app_with_local_deck_index(vec![second, first]);
    app.update(Msg::Key(key(KeyCode::Char('3'))));
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::LocalRow(0));

    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("One More Time")
    );
    assert!(
        load_url(&cmds)
            .expect("right-click album load")
            .contains("01 One")
    );
}

#[test]
fn local_deck_switch_stops_playback_and_restores_cached_queues() {
    let mut app = app_playing(3, 1);
    app.playback.paused = false;
    app.streaming.pending = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id1".to_owned(),
        shortlist: Vec::new(),
        local_pick: Vec::new(),
        cid_map: Vec::new(),
        mode: crate::streaming::config::StreamingMode::Balanced,
        cache_key: 42,
    });

    let enter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert!(app.local_dedicated_mode);
    assert_eq!(app.mode, Mode::Library);
    assert!(has_stop(&enter), "entering Local Deck should stop mpv");
    assert!(app.queue.is_empty());
    assert!(load_url(&enter).is_none());
    assert!(!app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_none());

    app.queue
        .set(vec![local_song("local_alpha"), local_song("local_beta")], 1);
    app.load_song(app.queue.current().cloned());
    app.playback.paused = false;
    let exit = app.apply_local_mode_confirm(LocalModeConfirm::Exit);

    assert!(!app.local_dedicated_mode);
    assert!(has_stop(&exit), "leaving Local Deck should stop mpv");
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id1");
    assert!(
        load_url(&exit)
            .expect("restored normal load")
            .contains("id1")
    );
    assert!(!app.playback.paused);

    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());
    let reenter = app.apply_local_mode_confirm(LocalModeConfirm::Enter);

    assert!(app.local_dedicated_mode);
    assert!(has_stop(&reenter));
    assert_eq!(app.queue.len(), 2);
    assert_eq!(
        app.queue.current().map(|s| s.title.as_str()),
        Some("local_beta")
    );
    assert!(
        load_url(&reenter)
            .expect("restored local load")
            .contains("/tmp/local_beta.m4a")
    );
}

#[test]
fn local_deck_session_snapshot_and_restore_use_local_queue() {
    let mut app = app_playing(2, 1);
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    app.queue
        .set(vec![local_song("local_alpha"), local_song("local_beta")], 1);

    let cache = app.session_cache_snapshot();

    assert_eq!(cache.last_mode, crate::session::LastMode::Local);
    assert_eq!(cache.normal_queue.as_ref().map(|s| s.songs.len()), Some(2));
    assert_eq!(cache.local_queue.as_ref().map(|s| s.cursor), Some(1));

    let mut restored = App::new(100);
    restored.restore_last_session_from_cache(&cache);

    assert!(restored.local_dedicated_mode);
    assert_eq!(restored.mode, Mode::Library);
    assert_eq!(restored.queue.len(), 2);
    assert_eq!(
        restored.queue.current().map(|s| s.title.as_str()),
        Some("local_beta")
    );
    assert!(restored.playback.paused);
}

#[test]
fn restoring_empty_local_session_does_not_fall_back_to_normal_history() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    let cache = crate::session::SessionCache::from_last_mode(crate::session::LastMode::Local);

    app.restore_last_session_from_cache(&cache);

    assert!(app.local_dedicated_mode);
    assert!(app.queue.is_empty());
}

#[test]
fn radio_mode_switch_stops_playback_restores_cached_queues_and_themes() {
    let mut app = app_playing(3, 1);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    app.playback.paused = false;
    app.streaming.pending = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id1".to_owned(),
        shortlist: Vec::new(),
        local_pick: Vec::new(),
        cid_map: Vec::new(),
        mode: crate::streaming::config::StreamingMode::Balanced,
        cache_key: 42,
    });

    let enter = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    assert!(app.radio_dedicated_mode);
    assert!(has_stop(&enter), "entering Radio mode should stop mpv");
    assert!(app.queue.is_empty());
    assert!(app.playback.paused);
    assert!(load_url(&enter).is_none());
    assert!(!app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_none());
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

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
    assert_eq!(app.theme.preset, "rose_pine");
    assert_eq!(
        app.config.theme.preset, "midnight",
        "normal theme in config should survive Radio-mode theme edits"
    );
    assert_eq!(
        app.config.radio_theme.as_ref().map(|t| t.preset.as_str()),
        Some("rose_pine"),
        "a Radio-mode theme edit should persist into its own config slot"
    );

    app.apply_radio_mode_confirm(RadioModeConfirm::Exit);
    assert_eq!(app.theme.preset, "midnight");
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert_eq!(app.theme.preset, "rose_pine");
}

#[test]
fn persisted_radio_theme_survives_restart_into_radio_session() {
    // Quit while in radio mode with a saved radio theme, relaunch: the session restore
    // must find the persisted radio theme instead of falling back to Radio.
    let mut cfg = crate::config::Config::default();
    let mut radio_theme = crate::theme::ThemeConfig::default();
    radio_theme.set_preset(crate::theme::ThemePreset::RosePine);
    cfg.radio_theme = Some(radio_theme);

    let mut app = App::new(100);
    app.apply_config(&cfg);
    app.library.record_play(&radio_station("latest"));
    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "rose_pine");
}

#[test]
fn persisted_radio_theme_applies_on_radio_reentry_after_relaunch() {
    // Quit in NORMAL mode (radio theme saved earlier), relaunch, then re-enter radio
    // mode: the stash seeded from config must win over the Radio fallback, and exiting
    // must return to the normal theme untouched.
    let mut cfg = crate::config::Config::default();
    let mut normal = crate::theme::ThemeConfig::default();
    normal.set_preset(crate::theme::ThemePreset::Midnight);
    cfg.theme = normal;
    let mut radio_theme = crate::theme::ThemeConfig::default();
    radio_theme.set_preset(crate::theme::ThemePreset::RosePine);
    cfg.radio_theme = Some(radio_theme);

    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.theme.preset, "midnight");

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert_eq!(app.theme.preset, "rose_pine");
    app.apply_radio_mode_confirm(RadioModeConfirm::Exit);
    assert_eq!(app.theme.preset, "midnight");
}

#[test]
fn settings_enqueue_next_toggle_persists_on_close() {
    let mut app = App::new(100);
    app.open_settings();
    let row = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::EnqueueNext)
        .expect("enqueue-next setting");
    app.settings.as_mut().unwrap().row = row;

    app.settings_change(1);
    assert!(app.settings.as_ref().unwrap().draft.enqueue_next);
    let cmds = app.close_settings();

    assert!(app.config.effective_enqueue_next());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn radio_mode_nav_labels_player_as_radio_without_shifting_tabs() {
    let _guard = crate::i18n::lock_for_test();
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
    let normal_buttons = app.hits.regions();
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
    let radio_buttons = app.hits.regions();
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
fn radio_mode_renders_custom_radio_art() {
    let mut app = App::new(100);
    // The set piece rides the album-art toggle (off by default).
    app.config.album_art = Some(true);

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        radio_text.contains("⢸⣿⣿⣉⣉⣉⣹⣿"),
        "radio mode should render the custom radio art"
    );
}

#[test]
fn radio_separator_renders_only_in_radio_mode() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);

    let normal = render_app_buffer(&app, 100, 36);
    let normal_text: String = normal
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        !normal_text.contains("♫♪.ılılı"),
        "normal player mode should not render the radio separator"
    );

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        radio_text.contains("♫♪.ılılı"),
        "radio mode should render the separator inside the player border"
    );
}

#[test]
fn radio_art_animates_when_animation_master_is_on() {
    let mut app = App::new(100);
    // The set piece rides the album-art toggle (off by default).
    app.config.album_art = Some(true);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("moving")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;

    assert!(
        app.animation_active(),
        "radio art should wake the animation clock when the master switch is on"
    );

    app.anim.anim_frame = 0;
    let first = render_app_buffer(&app, 80, 24);
    let first_text: String = first
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    app.anim.anim_frame = 24;
    let later = render_app_buffer(&app, 80, 24);
    let later_text: String = later
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert_ne!(
        first_text, later_text,
        "radio mode art should move on a slower animation phase"
    );
}

/// Read one buffer row as a string of cell symbols (index == column, one symbol per cell).
fn buffer_row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn radio_art_hidden_when_album_art_disabled() {
    let mut app = App::new(100);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("plain")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;

    let radio = render_app_buffer(&app, 80, 24);
    let text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        !text.contains("⢸⣿⣿⣉⣉⣉⣹⣿"),
        "album art off must hide the radio set piece"
    );
    assert!(
        !text.contains("♫♪.ılılı"),
        "album art off must hide the one-line art too"
    );
    assert!(
        !app.animation_active(),
        "with the set piece hidden and no effects enabled the clock must stay asleep"
    );
}

#[test]
fn radio_mode_keeps_gap_and_animates_canvas_below_separator() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("canvas")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.rain = true;

    app.anim.anim_frame = 0;
    let first = render_app_buffer(&app, 100, 36);
    let sep_y = (0..36)
        .find(|&y| buffer_row(&first, y).contains("ılılı"))
        .expect("one-line art row");

    // Two luxury rows sit between the set piece's bottom edge and the one-line art
    // (the art's own blank-braille pad row is ⠀ glyphs, not spaces, so a collapsed gap
    // would show up here).
    for dy in 1..=2u16 {
        let interior: String = buffer_row(&first, sep_y - dy)
            .chars()
            .skip(1)
            .take(97)
            .collect();
        assert!(
            interior.trim().is_empty(),
            "row {dy} above the one-line art should be blank, got: {interior:?}"
        );
    }

    // The music-mode canvas (rain) animates in the blank band below the one-line art.
    app.anim.anim_frame = 40;
    let later = render_app_buffer(&app, 100, 36);
    let below = |buf: &ratatui::buffer::Buffer| -> String {
        (sep_y + 1..34).map(|y| buffer_row(buf, y)).collect()
    };
    assert_ne!(
        below(&first),
        below(&later),
        "the filler canvas below the one-line art should animate in radio mode"
    );
}

#[test]
fn toggle_animations_in_radio_mode_flips_radio_master_not_master() {
    let mut app = App::new(100);
    app.config.animations.master = true;
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert!(
        app.animations().master,
        "radio inherits the music master until first toggled"
    );

    let cmds = app.toggle_animations();

    assert!(
        app.config.animations.master,
        "the music-mode switch must stay untouched"
    );
    assert_eq!(app.config.animations.radio_master, Some(false));
    assert!(
        !app.animations().master,
        "radio mode now resolves to its own switch"
    );
    // The raw config (music master intact) is what gets persisted, never the resolved copy.
    assert!(matches!(
        &cmds[..],
        [Cmd::Persist(PersistCmd::Config(c))] if c.animations.master && c.animations.radio_master == Some(false)
    ));

    app.radio_dedicated_mode = false;
    assert!(
        app.animations().master,
        "music mode keeps animating independently of the radio switch"
    );
}

#[test]
fn double_clicking_active_player_tab_confirms_radio_mode() {
    let mut app = App::new(100);

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Player));

    assert!(cmds.is_empty());
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

#[test]
fn autoplay_streaming_does_not_extend_from_radio_browser_streams() {
    let mut app = App::new(100);
    app.autoplay_streaming = true;
    app.queue.set(vec![radio_station("station-seed")], 0);
    app.mode = Mode::Player;

    let cmds = app.load_song(app.queue.current().cloned());

    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::StreamingFallback { .. }))
    );
    assert!(app.library.history.is_empty());
    assert_eq!(app.library.radios.len(), 1);
}

#[test]
fn load_song_reapplies_active_eq_chain() {
    let mut app = app_playing(3, 0);
    app.audio.bands = EqPreset::BassBoost.gains();
    // A manual skip reloads the track and must re-send the EQ chain (gapless rebuild
    // can otherwise drop the labeled bands).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
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
        autoplay_streaming: Some(true),
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
    // Music-mode invariant: this config carries both repeat and autoplay on, which can't both be
    // on — apply_config keeps the deliberate repeat and drops streaming.
    assert!(!app.autoplay_streaming);
}

#[test]
fn apply_config_with_autoplay_and_no_repeat_keeps_streaming() {
    // The reconcile only fires when both are on: autoplay alone still pushes through.
    let cfg = crate::config::Config {
        repeat: crate::queue::Repeat::Off,
        autoplay_streaming: Some(true),
        ..crate::config::Config::default()
    };
    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert!(app.autoplay_streaming);
    assert!(app.streaming_active());
}

#[test]
fn cannot_enable_streaming_while_repeat_on() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::All;
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_streaming, "streaming stays off");
    assert!(!app.status.text.is_empty(), "a message is shown");
    assert!(save_config(&cmds).is_none(), "nothing persisted");
    assert_eq!(
        app.queue.repeat,
        crate::queue::Repeat::All,
        "repeat untouched"
    );
}

#[test]
fn cannot_enable_repeat_while_streaming_on() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(
        app.queue.repeat,
        crate::queue::Repeat::Off,
        "repeat stays off"
    );
    assert!(app.autoplay_streaming, "streaming untouched");
    assert!(!app.status.text.is_empty(), "a message is shown");
    assert!(save_config(&cmds).is_none(), "nothing persisted");
}

#[test]
fn streaming_toggle_in_radio_mode_keeps_preference() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true; // a real preference carried from music mode
    app.radio_dedicated_mode = true;
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming, "stored preference is preserved");
    assert!(
        !app.streaming_active(),
        "but streaming is effectively off in radio mode"
    );
    assert!(!app.status.text.is_empty(), "a message explains why");
    assert!(
        save_config(&cmds).is_none(),
        "no persist — the preference is untouched"
    );
}

#[test]
fn streaming_active_false_in_radio_and_on_a_station() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true;
    assert!(app.streaming_active());
    app.radio_dedicated_mode = true;
    assert!(!app.streaming_active(), "off in dedicated Radio mode");
    app.radio_dedicated_mode = false;
    assert!(app.streaming_active());
    // A live station playing in normal mode also suppresses it.
    let mut radio = radio_playing("groove");
    radio.autoplay_streaming = true;
    assert!(radio.current_is_radio_stream());
    assert!(!radio.streaming_active(), "off while a live station plays");
}

#[test]
fn settings_cannot_enable_autoplay_while_repeat_on() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::All;
    app.open_settings();
    {
        let s = app.settings.as_mut().unwrap();
        s.tab = crate::settings::SettingsTab::Ai;
        s.row = s
            .fields()
            .iter()
            .position(|f| *f == Field::AutoplayStreaming)
            .expect("an AutoplayStreaming field");
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::AutoplayStreaming)
    );
    assert!(!app.settings.as_ref().unwrap().draft.autoplay_streaming);
    app.settings_change(1);
    assert!(
        !app.settings.as_ref().unwrap().draft.autoplay_streaming,
        "draft not flipped while repeat is on"
    );
    assert!(!app.status.text.is_empty(), "a message is shown");
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
        Cmd::Persist(PersistCmd::Config(c)) => Some(c.as_ref()),
        _ => None,
    })
}

fn focus_settings_field(app: &mut App, tab: SettingsTab, field: Field) {
    if app.settings.is_none() {
        app.open_settings();
    }
    let st = app.settings.as_mut().expect("settings open");
    st.tab = tab;
    st.row = st
        .fields()
        .iter()
        .position(|f| *f == field)
        .unwrap_or_else(|| panic!("{field:?} is visible on {tab:?}"));
    st.editing_text = false;
    st.capturing = None;
    assert_eq!(st.current_field(), Some(field));
}

#[test]
fn settings_search_provider_toggles_normalize_selected_sources() {
    let mut app = App::new(100);
    app.open_settings();
    {
        let draft = &mut app.settings.as_mut().unwrap().draft.search;
        draft.source = SearchSource::Youtube;
        draft.streaming_source = SearchSource::Youtube;
        draft.youtube = true;
        draft.soundcloud = true;
    }

    focus_settings_field(&mut app, SettingsTab::General, Field::SearchYoutube);
    app.settings_change(1);

    let draft = &app.settings.as_ref().unwrap().draft.search;
    assert!(!draft.youtube);
    assert_eq!(draft.source, SearchSource::SoundCloud);
    assert_eq!(draft.streaming_source, SearchSource::SoundCloud);

    focus_settings_field(&mut app, SettingsTab::General, Field::SearchYoutube);
    app.settings_change(1);
    assert!(app.settings.as_ref().unwrap().draft.search.youtube);
}

#[test]
fn settings_playback_changes_emit_live_player_commands() {
    let mut app = App::new(100);
    app.open_settings();

    focus_settings_field(&mut app, SettingsTab::Playback, Field::Speed);
    let cmds = app.settings_change(1);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetProperty { name, value })]
            if name == "speed" && value.as_f64().is_some_and(|v| v > 1.0)
    ));

    focus_settings_field(&mut app, SettingsTab::Playback, Field::Normalize);
    let cmds = app.settings_change(1);
    assert!(
        matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::SetAudioFilter(_))]),
        "normalize rebuilds the audio-filter chain"
    );

    focus_settings_field(&mut app, SettingsTab::Playback, Field::Band(0));
    let cmds = app.settings_change(1);
    assert!(
        matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::SetAudioFilter(_))]),
        "first non-zero EQ band creates the filter chain"
    );
    let cmds = app.settings_change(1);
    assert!(
        matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::AfCommand { .. })]),
        "subsequent active EQ edits update the labeled band"
    );
}

#[test]
fn settings_text_fields_persist_provider_ids_and_download_dir() {
    let mut app = App::new(100);
    app.open_settings();

    focus_settings_field(&mut app, SettingsTab::General, Field::AudiusAppName);
    app.settings.as_mut().unwrap().draft.search.audius_app_name = Some("  custom-app  ".to_owned());
    let cmds = app.settings_persist_text_field(Field::AudiusAppName);
    assert_eq!(
        app.config.search.audius_app_name.as_deref(),
        Some("custom-app")
    );
    assert!(save_config(&cmds).is_some());

    focus_settings_field(&mut app, SettingsTab::General, Field::JamendoClientId);
    app.settings
        .as_mut()
        .unwrap()
        .draft
        .search
        .jamendo_client_id = Some("  ".to_owned());
    let cmds = app.settings_persist_text_field(Field::JamendoClientId);
    assert!(app.config.search.jamendo_client_id.is_none());
    assert!(save_config(&cmds).is_some());

    let new_dir = std::env::temp_dir().join(format!("ytt-downloads-{}", std::process::id()));
    focus_settings_field(&mut app, SettingsTab::General, Field::DownloadDir);
    app.settings.as_mut().unwrap().draft.download_dir = new_dir.display().to_string();
    let cmds = app.settings_persist_text_field(Field::DownloadDir);
    assert_eq!(app.config.download_dir.as_deref(), Some(new_dir.as_path()));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SetDownloadDir(path) if path == &new_dir))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::ScanDownloads(path) if path == &new_dir))
    );
    assert!(save_config(&cmds).is_some());
}

#[test]
fn settings_local_music_root_persists_and_rescans_active_local_deck() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/ytt-local-downloads");
    let music = PathBuf::from("/tmp/ytt-local-library");
    app.config.download_dir = Some(downloads.clone());
    app.local_dedicated_mode = true;
    app.open_settings();

    focus_settings_field(&mut app, SettingsTab::General, Field::LocalMusicRoot);
    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.local_music_root = music.display().to_string();
        draft.local_music_root_recursive = false;
    }

    let cmds = app.settings_persist_text_field(Field::LocalMusicRoot);

    assert_eq!(app.config.local.roots.len(), 1);
    assert_eq!(app.config.local.roots[0].path, music);
    assert!(!app.config.local.roots[0].recursive());
    let Some(Cmd::Local(LocalCmd::ScanRoots { roots, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan after changing the music root");
    };
    assert_eq!(
        roots,
        &vec![
            crate::local::LocalScanRoot::download(downloads),
            crate::local::LocalScanRoot {
                path: PathBuf::from("/tmp/ytt-local-library"),
                recursive: false,
            },
        ]
    );
    assert!(save_config(&cmds).is_some());
}

#[test]
fn closing_settings_with_local_root_toggles_rescans_active_local_deck() {
    let mut app = App::new(100);
    let downloads = PathBuf::from("/tmp/ytt-close-downloads");
    let music = PathBuf::from("/tmp/ytt-close-library");
    app.config.download_dir = Some(downloads);
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: music.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];
    app.local_dedicated_mode = true;
    app.open_settings();
    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.local_include_download_dir = false;
        draft.local_music_root_recursive = false;
    }

    let cmds = app.close_settings();

    assert!(!app.config.local.include_download_dir());
    assert!(!app.config.local.roots[0].recursive());
    let Some(Cmd::Local(LocalCmd::ScanRoots { roots, .. })) = cmds
        .iter()
        .find(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    else {
        panic!("expected Local Deck rescan after changing local root toggles");
    };
    assert_eq!(
        roots,
        &vec![crate::local::LocalScanRoot {
            path: music,
            recursive: false,
        }]
    );
}

#[test]
fn settings_recording_popup_adjusts_sliders_toggles_and_text() {
    let mut app = App::new(100);
    app.open_settings();
    app.overlays.recording_settings = Some(RecordingSettingsPopup::default());

    app.overlays.recording_settings.as_mut().unwrap().row = 1;
    let old_min = app.settings.as_ref().unwrap().draft.recording_min_seconds;
    app.recording_settings_adjust(1);
    assert!(app.settings.as_ref().unwrap().draft.recording_min_seconds > old_min);

    app.overlays.recording_settings.as_mut().unwrap().row = 5;
    let old_notify = app.settings.as_ref().unwrap().draft.recording_notify;
    app.recording_settings_adjust(1);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_notify,
        !old_notify
    );

    app.overlays.recording_settings.as_mut().unwrap().row = 3;
    app.recording_settings_confirm();
    assert!(
        app.overlays
            .recording_settings
            .as_ref()
            .unwrap()
            .editing_dir
    );
    app.recording_settings_key(key(KeyCode::Char('x')));
    app.recording_settings_key(key(KeyCode::Backspace));
    app.recording_settings_key(key(KeyCode::Enter));
    assert!(
        !app.overlays
            .recording_settings
            .as_ref()
            .unwrap()
            .editing_dir
    );

    app.recording_slider_set(4, 9, ratatui::layout::Rect::new(0, 0, 10, 1));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        crate::config::RECORDING_PAST_TRACKS_MAX
    );
}

#[test]
fn recordings_browser_moves_saves_discards_and_closes() {
    let _guard = crate::i18n::lock_for_test();
    use crate::recorder::job::RecorderJob;
    use crate::recorder::{RecordedTrack, RecordingState};

    let mut app = App::new(100);
    app.overlays.recordings_browser = Some(RecordingsBrowser::default());
    app.recorder.history.push_back(RecordedTrack {
        id: 10,
        title: Some("One".to_owned()),
        artist: Some("Artist".to_owned()),
        raw: "Artist - One".to_owned(),
        station: Some("Station".to_owned()),
        temp_path: std::path::PathBuf::from("/tmp/one.mp3"),
        ext: "mp3",
        duration_secs: 61,
        state: RecordingState::Recorded,
        final_path: None,
    });
    app.recorder.history.push_back(RecordedTrack {
        id: 20,
        title: Some("Two".to_owned()),
        artist: Some("Artist".to_owned()),
        raw: "Artist - Two".to_owned(),
        station: Some("Station".to_owned()),
        temp_path: std::path::PathBuf::from("/tmp/two.mp3"),
        ext: "mp3",
        duration_secs: 122,
        state: RecordingState::RecordedReachedMaxDuration,
        final_path: None,
    });
    assert_eq!(app.recordings_browser_ids(), vec![10, 20]);

    let cmds = app.recordings_browser_key(key(KeyCode::Down));
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.recordings_browser.as_ref().unwrap().selected,
        1
    );

    let cmds = app.recordings_browser_key(key(KeyCode::Enter));
    assert!(cmds.is_empty());
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "Save the track first to play it");

    let cmds = app.recordings_browser_key(key(KeyCode::Char('s')));
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Recorder(RecorderJob::Save { id, filename, ext, title, artist, .. })]
                if *id == 20
                    && filename == "Artist - Two"
                    && *ext == "mp3"
                    && title.as_deref() == Some("Two")
                    && artist.as_deref() == Some("Artist")
        ),
        "saving the selected recording should enqueue an off-loop save job"
    );
    assert_eq!(app.recorder.history[1].state, RecordingState::Saved);

    let cmds = app.recordings_browser_key(key(KeyCode::Char('d')));
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Recorder(RecorderJob::Discard { temp })]
                if temp == &std::path::PathBuf::from("/tmp/two.mp3")
        ),
        "discarding removes the selected history row and deletes only its temp file"
    );
    assert_eq!(app.recordings_browser_ids(), vec![10]);

    let cmds = app.recordings_browser_key(key(KeyCode::Esc));
    assert!(cmds.is_empty());
    assert!(app.overlays.recordings_browser.is_none());
}

#[test]
fn settings_change_updates_stored_selectors_and_toggles_across_tabs() {
    let mut app = App::new(100);
    app.open_settings();

    macro_rules! change_stored {
        ($tab:expr, $field:expr) => {{
            focus_settings_field(&mut app, $tab, $field);
            let cmds = app.settings_change(1);
            assert!(cmds.is_empty(), "{:?} emitted {} cmds", $field, cmds.len());
        }};
    }

    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.search.source = SearchSource::Youtube;
        draft.search.streaming_source = SearchSource::Youtube;
        draft.search.youtube = true;
        draft.search.soundcloud = true;
        draft.search.audius = true;
        draft.search.jamendo = true;
        draft.search.internet_archive = true;
        draft.search.radio_browser = true;
        draft.mouse = true;
        draft.album_art = true;
        draft.big_text = false;
        draft.autoplay_on_start = false;
        draft.enqueue_next = true;
        draft.update_check_enabled = true;
        draft.seek_seconds = 10.0;
        draft.mouse_wheel_volume = true;
        draft.gapless = true;
        draft.media_controls = true;
        draft.auto_continue_videos = false;
        draft.video_layout = crate::config::VideoOverlay::Compact;
        draft.gemini_model = crate::ai::GeminiModel::FlashLite;
        draft.ai_enabled = true;
        draft.romanized_titles = false;
        draft.dj_gem_language = crate::i18n::DjGemLanguage::Auto;
        draft.autoplay_streaming = false;
        draft.curating_mode = crate::streaming::CuratingMode::DjGem;
        draft.streaming_mode = crate::streaming::StreamingMode::Balanced;
        draft.theme.set_preset(crate::theme::ThemePreset::Default);
        draft.animations.fps = crate::config::FPS_DEFAULT;
        draft.animations.pause_unfocused = true;
        draft.animations.master = false;
        draft.animations.title = false;
        draft.animations.bounce = false;
        draft.lastfm_enabled = true;
        draft.lastfm_love_sync = true;
        draft.listenbrainz_enabled = true;
        draft.scrobble_local_files = false;
    }
    change_stored!(SettingsTab::General, Field::SearchSource);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.search.source,
        SearchSource::SoundCloud
    );
    change_stored!(SettingsTab::General, Field::StreamingSource);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.search.streaming_source,
        SearchSource::SoundCloud
    );

    change_stored!(SettingsTab::General, Field::Mouse);
    assert!(!app.settings.as_ref().unwrap().draft.mouse);
    change_stored!(SettingsTab::General, Field::AlbumArt);
    assert!(!app.settings.as_ref().unwrap().draft.album_art);
    change_stored!(SettingsTab::General, Field::BigText);
    assert!(app.settings.as_ref().unwrap().draft.big_text);
    change_stored!(SettingsTab::General, Field::AutoplayOnStart);
    assert!(app.settings.as_ref().unwrap().draft.autoplay_on_start);
    change_stored!(SettingsTab::General, Field::EnqueueNext);
    assert!(!app.settings.as_ref().unwrap().draft.enqueue_next);
    change_stored!(SettingsTab::General, Field::UpdateCheck);
    assert!(!app.settings.as_ref().unwrap().draft.update_check_enabled);

    change_stored!(SettingsTab::General, Field::SearchSoundCloud);
    let search = &app.settings.as_ref().unwrap().draft.search;
    assert!(!search.soundcloud);
    assert_ne!(search.source, SearchSource::SoundCloud);
    assert_ne!(search.streaming_source, SearchSource::SoundCloud);
    change_stored!(SettingsTab::General, Field::SearchAudius);
    assert!(!app.settings.as_ref().unwrap().draft.search.audius);
    change_stored!(SettingsTab::General, Field::SearchJamendo);
    assert!(!app.settings.as_ref().unwrap().draft.search.jamendo);
    change_stored!(SettingsTab::General, Field::SearchInternetArchive);
    assert!(!app.settings.as_ref().unwrap().draft.search.internet_archive);
    change_stored!(SettingsTab::General, Field::SearchRadioBrowser);
    assert!(!app.settings.as_ref().unwrap().draft.search.radio_browser);

    change_stored!(SettingsTab::Playback, Field::SeekInterval);
    assert_eq!(app.settings.as_ref().unwrap().draft.seek_seconds, 11.0);
    change_stored!(SettingsTab::Playback, Field::MouseWheelVolume);
    assert!(!app.settings.as_ref().unwrap().draft.mouse_wheel_volume);
    change_stored!(SettingsTab::Playback, Field::Gapless);
    assert!(!app.settings.as_ref().unwrap().draft.gapless);
    change_stored!(SettingsTab::Playback, Field::MediaControls);
    assert!(!app.settings.as_ref().unwrap().draft.media_controls);
    change_stored!(SettingsTab::Playback, Field::AutoContinueVideos);
    assert!(app.settings.as_ref().unwrap().draft.auto_continue_videos);
    change_stored!(SettingsTab::Playback, Field::VideoLayout);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.video_layout,
        crate::config::VideoOverlay::Large
    );

    change_stored!(SettingsTab::Ai, Field::GeminiModel);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.gemini_model,
        crate::ai::GeminiModel::Latest
    );
    change_stored!(SettingsTab::Ai, Field::AiEnabled);
    assert!(!app.settings.as_ref().unwrap().draft.ai_enabled);
    change_stored!(SettingsTab::Ai, Field::RomanizedTitles);
    assert!(app.settings.as_ref().unwrap().draft.romanized_titles);
    change_stored!(SettingsTab::Ai, Field::DjGemLanguage);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.dj_gem_language,
        crate::i18n::DjGemLanguage::English
    );
    change_stored!(SettingsTab::Ai, Field::AutoplayStreaming);
    assert!(app.settings.as_ref().unwrap().draft.autoplay_streaming);
    change_stored!(SettingsTab::Ai, Field::CuratingMode);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.curating_mode,
        crate::streaming::CuratingMode::YtNative
    );
    change_stored!(SettingsTab::Ai, Field::StreamingMode);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.streaming_mode,
        crate::streaming::StreamingMode::Discovery
    );

    change_stored!(SettingsTab::Graphics, Field::ThemePreset);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.theme.preset_enum(),
        crate::theme::ThemePreset::Midnight
    );
    assert_eq!(app.theme.preset_enum(), crate::theme::ThemePreset::Midnight);
    change_stored!(SettingsTab::Graphics, Field::BackgroundNone);
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .theme
            .is_role_transparent(crate::theme::ThemeRole::Background)
    );
    change_stored!(SettingsTab::Graphics, Field::AnimFps);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.animations.fps,
        crate::config::FPS_DEFAULT + settings::ANIM_FPS_STEP
    );
    change_stored!(SettingsTab::Graphics, Field::AnimPauseUnfocused);
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .draft
            .animations
            .pause_unfocused
    );
    change_stored!(SettingsTab::Graphics, Field::AnimMaster);
    assert!(app.settings.as_ref().unwrap().draft.animations.master);
    change_stored!(SettingsTab::Graphics, Field::AnimTitle);
    assert!(app.settings.as_ref().unwrap().draft.animations.title);
    change_stored!(SettingsTab::Graphics, Field::AnimBounce);
    assert!(app.settings.as_ref().unwrap().draft.animations.bounce);

    change_stored!(SettingsTab::Accounts, Field::LastfmEnabled);
    assert!(!app.settings.as_ref().unwrap().draft.lastfm_enabled);
    change_stored!(SettingsTab::Accounts, Field::LastfmLoveSync);
    assert!(!app.settings.as_ref().unwrap().draft.lastfm_love_sync);
    change_stored!(SettingsTab::Accounts, Field::ListenBrainzEnabled);
    assert!(!app.settings.as_ref().unwrap().draft.listenbrainz_enabled);
    change_stored!(SettingsTab::Accounts, Field::ScrobbleLocalFiles);
    assert!(app.settings.as_ref().unwrap().draft.scrobble_local_files);
}

#[test]
fn spotify_picker_keyboard_confirm_maps_liked_to_likes_and_playlists_to_local() {
    use crate::transfer::actor::PickerPlaylist;

    let mut app = App::new(100);
    app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker {
        selected: 0,
        items: vec![
            PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyLiked,
                label: "Liked Songs".to_owned(),
                total: 100,
            },
            PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyPlaylist {
                    id: "pl-1".to_owned(),
                },
                label: "Roadtrip".to_owned(),
                total: 42,
            },
        ],
    });

    let cmds = app.spotify_picker_confirm();
    assert!(app.overlays.spotify_picker.is_none());
    assert!(app.transfer_running);
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(spec))
                if matches!(spec.source, crate::transfer::TransferSource::SpotifyLiked)
                    && matches!(spec.dest, crate::transfer::TransferDest::YtmLikes)
        )),
        "liked songs import should target YTM likes"
    );

    app.transfer_running = false;
    app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker {
        selected: 1,
        items: vec![
            PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyLiked,
                label: "Liked Songs".to_owned(),
                total: 100,
            },
            PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyPlaylist {
                    id: "pl-1".to_owned(),
                },
                label: "Roadtrip".to_owned(),
                total: 42,
            },
        ],
    });
    let cmds = app.spotify_picker_key(key(KeyCode::Enter));
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(spec))
                if matches!(spec.source, crate::transfer::TransferSource::SpotifyPlaylist { .. })
                    && matches!(spec.dest, crate::transfer::TransferDest::LocalPlaylist { name: None })
        )),
        "playlist import should target a local library playlist"
    );
}

#[test]
fn spotify_settings_connect_button_uses_draft_state_without_token_io() {
    let _guard = crate::i18n::lock_for_test();
    use crate::transfer::actor::TransferCmd;

    let mut app = App::new(100);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::Accounts, Field::SpotifyConnect);

    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.spotify_connected = true;
        draft.spotify_stale = false;
        draft.spotify_client_id = "client-a".to_owned();
    }
    let cmds = app.settings_activate();
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::SpotifyDisconnect),
        "a healthy saved connection should ask before disconnecting"
    );

    app.overlays.pending_settings_confirm = None;
    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.spotify_connected = false;
        draft.spotify_stale = false;
        draft.spotify_client_id.clear();
    }
    let cmds = app.settings_activate();
    assert!(cmds.is_empty());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("Set a Client ID first"));

    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.spotify_client_id = "client-b".to_owned();
        draft.spotify_redirect_port = "49152".to_owned();
    }
    let cmds = app.settings_activate();
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Transfer(TransferCmd::AuthStart { client_id, port })]
                if client_id == "client-b" && *port == 49152
        ),
        "a disconnected row with a client id should start browser auth"
    );
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Starting Spotify authorization"));

    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.spotify_connected = true;
        draft.spotify_stale = true;
        draft.spotify_client_id = "client-c".to_owned();
        draft.spotify_redirect_port = "not-a-port".to_owned();
    }
    let cmds = app.settings_activate();
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Transfer(TransferCmd::AuthStart { client_id, port })]
                if client_id == "client-c"
                    && *port == crate::config::SPOTIFY_REDIRECT_PORT_DEFAULT
        ),
        "a stale connection should reconnect, falling back to the default port on bad input"
    );
    assert!(app.status.text.contains("Reconnecting Spotify"));
}

#[test]
fn account_buttons_start_lastfm_auth_or_cancel_spotify_imports() {
    let _guard = crate::i18n::lock_for_test();
    use crate::transfer::actor::TransferCmd;

    let mut app = App::new(100);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::Accounts, Field::LastfmConnect);

    let cmds = app.settings_activate();
    assert!(matches!(cmds.as_slice(), [Cmd::ScrobbleAuthStart]));
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Requesting Last.fm authorization"));

    {
        let draft = &mut app.settings.as_mut().unwrap().draft;
        draft.lastfm_session_key = "session".to_owned();
        draft.lastfm_username = "listener".to_owned();
    }
    app.config.scrobble.lastfm.session_key = Some("session".to_owned());
    app.config.scrobble.lastfm.username = Some("listener".to_owned());

    let cmds = app.settings_activate();
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::LastfmDisconnect)
    );
    let cmds = app.settings_apply_confirm(SettingsConfirm::LastfmDisconnect);
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .lastfm_session_key
            .is_empty()
    );
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .lastfm_username
            .is_empty()
    );
    assert!(app.config.scrobble.lastfm.session_key.is_none());
    assert!(app.config.scrobble.lastfm.username.is_none());
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::ScrobbleReconfigure(_)))
    );

    focus_settings_field(&mut app, SettingsTab::Accounts, Field::SpotifyImport);
    app.transfer_running = true;
    let cmds = app.settings_activate();
    assert!(
        matches!(cmds.as_slice(), [Cmd::Transfer(TransferCmd::CancelJob)]),
        "while a transfer is running, the import button becomes cancel"
    );
    assert!(!app.transfer_running);
    assert!(app.status.text.contains("Cancelling the import"));
}

#[test]
fn transfer_events_surface_playlist_progress_and_failures() {
    let _guard = crate::i18n::lock_for_test();
    use crate::transfer::actor::{PickerPlaylist, TransferEvent};
    use crate::transfer::{Stage, TransferProgress, TransferSource};

    let mut app = App::new(100);
    let cmds = app.update(Msg::Transfer(TransferEvent::SpotifyPlaylists(Ok(
        Vec::new(),
    ))));
    assert!(cmds.is_empty());
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "No Spotify playlists");
    assert!(app.overlays.spotify_picker.is_none());

    app.update(Msg::Transfer(TransferEvent::SpotifyPlaylists(Ok(vec![
        PickerPlaylist {
            source: TransferSource::SpotifyLiked,
            label: "Liked Songs".to_owned(),
            total: 0,
        },
        PickerPlaylist {
            source: TransferSource::SpotifyPlaylist {
                id: "pl-road".to_owned(),
            },
            label: "Roadtrip".to_owned(),
            total: 42,
        },
    ]))));
    let picker = app.overlays.spotify_picker.as_ref().expect("picker opens");
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.items.len(), 2);
    assert_eq!(picker.items[1].label, "Roadtrip");
    assert!(app.status.text.is_empty());

    app.update(Msg::Transfer(TransferEvent::SpotifyPlaylists(Err(
        "bad\u{1b}[31m token".to_owned(),
    ))));
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("Could not list Spotify playlists"));

    app.update(Msg::Transfer(TransferEvent::Progress(TransferProgress {
        job_id: "sp2yt-1".to_owned(),
        stage: Stage::Matching,
        done: 2,
        total: 5,
        matched: 1,
        ambiguous: 1,
        not_found: 0,
        current: "Artist - Song".to_owned(),
    })));
    assert!(app.transfer_running);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Spotify import: matching 2/5"));
    assert!(app.status.text.contains("Artist - Song"));

    app.update(Msg::Transfer(TransferEvent::JobFailed {
        job_id: "sp2yt-1".to_owned(),
        error: "rate limited".to_owned(),
        resumable: true,
    }));
    assert!(!app.transfer_running);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("Import interrupted"));
    assert!(app.status.text.contains("ytt transfer resume sp2yt-1"));

    app.transfer_running = true;
    app.update(Msg::Transfer(TransferEvent::JobFailed {
        job_id: String::new(),
        error: "boom".to_owned(),
        resumable: false,
    }));
    assert!(!app.transfer_running);
    assert_eq!(app.status.text, "Import failed: boom");

    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Accounts;
    app.settings.as_mut().unwrap().draft.spotify_connected = true;
    app.settings.as_mut().unwrap().draft.spotify_stale = true;
    app.settings.as_mut().unwrap().draft.spotify_username = "old".to_owned();
    app.update(Msg::Transfer(TransferEvent::Disconnected));
    let draft = &app.settings.as_ref().unwrap().draft;
    assert!(!draft.spotify_connected);
    assert!(!draft.spotify_stale);
    assert!(draft.spotify_username.is_empty());
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "Spotify disconnected");
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

    app.overlays.help_visible = true;
    assert!(
        app.animation_active(),
        "cheat-sheet overlay should not pause the background animation"
    );
    app.overlays.help_visible = false;

    app.overlays.about_visible = true;
    assert!(
        app.animation_active(),
        "About overlay should not pause the background animation"
    );
    app.overlays.about_visible = false;

    app.overlays.why_ai_visible = true;
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

// --- one-shot fx: central trigger detection ------------------------------

/// Every animation flag on, every one-shot mid-flight, every screen and overlay, several
/// terminal sizes (down to tiny), several frames into the windows, plus a retro pass — all
/// of it must render without panicking. This is the smoke net for the direct-cell overlay
/// effects (bursts, flashes, fades, sparkles), whose coordinate math is easiest to get wrong
/// at edge sizes.
#[test]
fn all_animations_on_render_every_view_without_panic() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(3, 0);
    app.playback.paused = false;
    app.playback.time_pos = Some(30.0);
    app.playback.time_pos_at = Some(Instant::now());
    app.playback.duration = Some(120.0);

    let a = &mut app.config.animations;
    a.master = true;
    a.title = true;
    a.heart = true;
    a.seekbar = true;
    a.spinner = true;
    a.eq_bars = true;
    a.controls = true;
    a.border = true;
    a.track_intro = true;
    a.lyrics = true;
    a.toast = true;
    a.volume_flash = true;
    a.like_burst = true;
    a.seek_flash = true;
    a.selection = true;
    a.stagger = true;
    a.caret = true;
    a.tabs = true;
    a.popup_fade = true;
    a.activity = true;
    a.about_fx = true;
    a.rain = true;
    a.donut = true;
    a.visualizer = true;
    a.starfield = true;
    a.bounce = true;
    assert!(app.config.animations.active());

    // Content for every effect to chew on.
    let cur = app.queue.current().unwrap().clone();
    app.library.toggle_favorite(&cur); // liked → heart + burst path
    app.lyrics.visible = true;
    app.lyrics.track = Some(TrackLyrics {
        video_id: cur.video_id.clone(),
        lines: (0..12)
            .map(|i| crate::lyrics::LyricLine {
                time: f64::from(i) * 5.0,
                text: format!("line {i}"),
            })
            .collect(),
    });
    app.downloads
        .active
        .insert(cur.video_id.clone(), DownloadState::Running(42));
    app.search.input = "abc".to_owned();
    app.search.results = songs(6);
    app.ai.input = "hey".to_owned();
    app.ai.thinking = true;
    app.ai.suggestions = songs(3);
    app.library_ui.filter_editing = true;
    app.library_ui.filter_query = "a".to_owned();
    app.status.text = "Saved: something nice".to_owned();

    // Arm every one-shot window by hand (the render side only reads the slots).
    app.fx.toast = Some(0);
    app.fx.track_intro = Some(0);
    app.fx.volume = Some(0);
    app.fx.like = Some(0);
    app.fx.seek = Some(0);
    app.fx.switch = Some((0, Mode::Player));
    app.fx.tabbar = Some(0);
    app.fx.list = Some((0, Mode::Library));
    app.fx.popup = Some(0);
    app.fx.lyric = Some(0);
    app.fx.until = u64::MAX;

    // Overlays stacked on top of whatever screen is active.
    app.overlays.help_visible = true;
    app.overlays.about_visible = true;
    app.queue_popup.open = true;
    app.playlist_picker = Some(PlaylistPicker {
        songs: vec![cur.clone()],
        cursor: 0,
        naming: Some("mix".to_owned()),
    });
    app.library_ui.create_input = Some("new list".to_owned());

    for retro in [false, true] {
        app.config.retro_mode = retro;
        for mode in [
            Mode::Player,
            Mode::Search,
            Mode::Library,
            Mode::Settings,
            Mode::Ai,
        ] {
            app.mode = mode;
            if mode == Mode::Settings && app.settings.is_none() {
                app.open_settings();
            }
            // Also point the cascade at this view so its stagger path runs.
            app.fx.list = Some((0, mode));
            for _ in 0..4 {
                // A few frames into every window (advances anim_frame via the real tick).
                app.update(Msg::AnimTick);
                let _ = render_app_buffer(&app, 80, 24);
                let _ = render_app_buffer(&app, 34, 10);
                let _ = render_app_buffer(&app, 12, 4);
            }
        }
    }
}

#[test]
fn volume_change_arms_the_volume_flash_from_any_path() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.volume_flash = true;
    app.update(Msg::Resize); // seed the diff anchors from launch state
    assert!(app.fx.volume.is_none(), "no phantom flash at startup");
    // The detection is a state diff, so it doesn't matter *which* path changed the volume
    // (key, wheel, remote) — any subsequent update sees it.
    app.playback.volume -= 5;
    app.update(Msg::Resize);
    assert!(app.fx.volume.is_some());
    assert!(app.fx_active(), "the one-shot keeps the clock awake");
    assert!(app.animation_active());
}

#[test]
fn fx_triggers_gate_on_master_and_flag() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Resize);
    // Flag on but master off → the anchor updates, nothing arms, clock stays asleep.
    app.config.animations.volume_flash = true;
    app.playback.volume -= 5;
    app.update(Msg::Resize);
    assert!(app.fx.volume.is_none());
    assert!(!app.fx_active());
    // Master on but this flag off → same.
    app.config.animations.master = true;
    app.config.animations.volume_flash = false;
    app.playback.volume -= 5;
    app.update(Msg::Resize);
    assert!(app.fx.volume.is_none());
    assert!(!app.fx_active());
}

#[test]
fn new_status_text_arms_the_toast_even_while_paused() {
    let mut app = app_playing(1, 0);
    app.playback.paused = true;
    app.config.animations.master = true;
    app.config.animations.toast = true;
    app.update(Msg::Resize);
    assert!(app.fx.toast.is_none());
    // A real reducer path that sets a status message.
    app.update(Msg::ApiModeResolved {
        mode: ApiMode::Anonymous,
        had_cookie: true,
    });
    assert!(!app.status.text.is_empty());
    assert!(app.fx.toast.is_some());
    assert!(
        app.animation_active(),
        "a one-shot wakes the clock even though playback is paused"
    );
}

#[test]
fn track_change_arms_the_intro_and_suppresses_the_like_burst() {
    let mut app = app_playing(2, 0);
    app.config.animations.master = true;
    app.config.animations.track_intro = true;
    app.config.animations.like_burst = true;
    // Pre-favorite the *next* track: when it becomes current, the liked flag flips — but as a
    // side effect of the track change, not a fresh like, so no burst.
    let next = app.queue.ordered_iter().nth(1).unwrap().clone();
    app.library.toggle_favorite(&next);
    app.update(Msg::Resize); // seed anchors (arms the intro for the launch track)
    app.fx.track_intro = None;
    app.queue.next(false);
    app.update(Msg::Resize);
    assert!(app.fx.track_intro.is_some(), "track change → intro cascade");
    assert!(
        app.fx.like.is_none(),
        "liked-flag flip via track change is not a like"
    );
    // A real like on the (unchanged) current track *does* burst.
    let cur = app.queue.current().unwrap().clone();
    app.library.toggle_favorite(&cur); // unlike (it was pre-favorited)
    app.update(Msg::Resize);
    assert!(app.fx.like.is_none(), "unliking never bursts");
    app.library.toggle_favorite(&cur); // like again
    app.update(Msg::Resize);
    assert!(app.fx.like.is_some());
}

#[test]
fn opening_a_popup_arms_the_fade_in_once() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.popup_fade = true;
    app.update(Msg::Resize);
    assert!(app.fx.popup.is_none());
    app.overlays.help_visible = true;
    app.update(Msg::Resize);
    assert!(app.fx.popup.is_some(), "newly-opened overlay bit → fade-in");
    // Still open on the next turn → no re-arm (the start frame is unchanged).
    let started = app.fx.popup;
    app.update(Msg::Resize);
    assert_eq!(app.fx.popup, started);
    // Closing arms nothing.
    app.overlays.help_visible = false;
    app.fx.popup = None;
    app.update(Msg::Resize);
    assert!(app.fx.popup.is_none());
}

#[test]
fn caret_and_ambient_effects_wake_the_clock_off_the_player() {
    let mut app = App::new(100);
    app.config.animations.master = true;
    app.config.animations.caret = true;
    assert!(
        !app.animation_active(),
        "player view has no text input — nothing to blink"
    );
    app.mode = Mode::Search;
    assert!(
        app.animation_active(),
        "the search box caret blinks with nothing playing at all"
    );
    app.config.animations.caret = false;
    assert!(!app.animation_active());
    // Activity dots while a search is in flight.
    app.config.animations.activity = true;
    app.search.searching = true;
    assert!(app.animation_active());
    app.search.searching = false;
    assert!(!app.animation_active());
    // The About card's sparkles animate over any screen.
    app.config.animations.about_fx = true;
    app.overlays.about_visible = true;
    assert!(app.animation_active());
    app.overlays.about_visible = false;
    assert!(!app.animation_active());
}

#[test]
fn ambient_effects_draw_at_a_lower_cadence_than_one_shots() {
    let mut app = App::new(100);
    app.config.animations.master = true;
    app.config.animations.caret = true;
    app.mode = Mode::Search;
    // Ambient-only (a blinking caret) redraws at a capped cadence…
    assert_eq!(app.animation_draw_fps(), 12);
    // …but a live one-shot window lifts drawing back to the full tick rate.
    app.config.animations.toast = true;
    app.update(Msg::ApiModeResolved {
        mode: ApiMode::Anonymous,
        had_cookie: true,
    });
    assert!(app.fx_active());
    assert_eq!(app.animation_draw_fps(), app.animation_tick_fps());
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
        assert_eq!(app.anim.anim_frame, expected_frame);
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
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert!(app.settings.is_some());
    assert!(!app.config.animations.master);
    // Toggle via the shared path (what both the `A` key and the ✨ click call).
    let cmds = app.toggle_animations();
    assert!(app.config.animations.master);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_)))),
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
fn settings_key_opens_and_q_closes_without_quitting() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Accounts);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General); // wraps
}

#[test]
fn settings_accounts_tab_renders_service_sections() {
    let mut app = App::new(100);
    app.config.retro_mode = true; // English labels for stable assertions
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    app.settings.as_mut().unwrap().tab = SettingsTab::Accounts;

    let buf = render_app_buffer(&app, 120, 40);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(text.contains("Last.fm"), "Last.fm section header renders");
    assert!(
        text.contains("ListenBrainz"),
        "ListenBrainz section renders"
    );
    assert!(text.contains("Spotify"), "Spotify section renders");
    assert!(
        text.contains("connect in browser"),
        "disconnected accounts offer the connect action"
    );
    assert!(
        text.contains("Client ID"),
        "Spotify Client ID field renders"
    );
}

#[test]
fn settings_whole_row_click_activates_a_button_row() {
    // B2: a click anywhere on a Button row (not only the value glyph) activates it. With no
    // Client ID set, activating the Spotify "connect" row surfaces the guidance message — proof
    // the click reached the handler rather than only focusing the row.
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.config.spotify.client_id = None; // force the empty-Client-ID connect path
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.settings.as_mut().unwrap().tab = SettingsTab::Accounts;
    let idx = app
        .settings
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| *f == Field::SpotifyConnect)
        .expect("a SpotifyConnect row");
    assert!(app.status.text.is_empty());
    let _ = app.on_list_row_click(idx);
    assert_eq!(
        app.settings.as_ref().unwrap().row,
        idx,
        "the row is focused"
    );
    // A whole-row click on a Button row activates it — the connect handler always sets a status
    // (empty-ID guidance, or a reconnect notice if a token is present). A focus-only click, the
    // old behaviour, would leave the status empty.
    assert!(
        !app.status.text.is_empty(),
        "a whole-row click activated the button (status still empty)"
    );
}

#[test]
fn spotify_picker_click_selects_then_confirms() {
    // C: the first click on a picker row selects it (no job yet); clicking the already-selected
    // row imports it — closing the picker and dispatching a transfer.
    use crate::transfer::actor::PickerPlaylist;
    let mut app = App::new(100);
    let items = vec![
        PickerPlaylist {
            source: crate::transfer::TransferSource::SpotifyLiked,
            label: "Liked Songs".to_owned(),
            total: 0,
        },
        PickerPlaylist {
            source: crate::transfer::TransferSource::SpotifyPlaylist {
                id: "abc".to_owned(),
            },
            label: "Roadtrip".to_owned(),
            total: 12,
        },
    ];
    app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker { items, selected: 0 });

    let cmds = app.on_mouse_target(MouseTarget::SpotifyPickRow(1));
    assert!(cmds.is_empty(), "selecting a new row doesn't start a job");
    assert_eq!(
        app.overlays.spotify_picker.as_ref().unwrap().selected,
        1,
        "the clicked row becomes selected"
    );

    let cmds = app.on_mouse_target(MouseTarget::SpotifyPickRow(1));
    assert!(
        app.overlays.spotify_picker.is_none(),
        "clicking the selected row closes the picker"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::Transfer(_))),
        "confirming dispatches a transfer job"
    );
}

#[test]
fn settings_keys_lists_radio_normal_mode_binding() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    assert!(
        text.contains("Enter / exit Local Deck"),
        "Keys tab should list the Local Deck mode binding"
    );
    assert!(
        text.contains("Alt+Shift+L"),
        "Keys tab should show the default Local Deck key"
    );
}

#[test]
fn help_overlay_shows_player_radio_normal_mode_binding() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.overlays.help_visible = true;

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
fn retro_frames_contain_only_cp437_safe_cells() {
    // The whole point of retro mode: whatever the app renders — CJK metadata, emoji
    // toggles, braille art, animation glyphs, the About icon — the scrubbed frame must
    // contain nothing a 256-glyph console font can't show.
    let mut app = App::new(100);
    app.queue.set(
        vec![crate::api::Song::remote(
            "vid1",
            "한글 제목 ♫",
            "アーティスト",
            "3:00",
        )],
        0,
    );
    app.mode = Mode::Player;
    app.config.retro_mode = true;
    app.queue.shuffle = true;
    app.queue.repeat = crate::queue::Repeat::One;
    app.config.animations.master = true;
    app.config.animations.spinner = true;
    app.config.animations.eq_bars = true;
    // Album art active (halfblocks fallback picker): retro must render it as ASCII art.
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);
    app.set_artwork(
        "vid1".to_owned(),
        Some(image::DynamicImage::new_rgb8(64, 64)),
    );

    let assert_scrubbed = |app: &App, label: &str| {
        let buf = render_app_buffer(app, 100, 30);
        for (i, cell) in buf.content().iter().enumerate() {
            assert!(
                crate::ui::retro::retro_supported(cell.symbol()),
                "{label}: cell {i} holds unsupported symbol {:?}",
                cell.symbol()
            );
        }
    };

    assert_scrubbed(&app, "player");
    app.overlays.about_visible = true;
    assert_scrubbed(&app, "about card");
    app.overlays.about_visible = false;
    app.overlays.help_visible = true;
    assert_scrubbed(&app, "help overlay");
    app.overlays.help_visible = false;
    app.mode = Mode::Search;
    assert_scrubbed(&app, "search");
    app.mode = Mode::Library;
    assert_scrubbed(&app, "library");
    app.mode = Mode::Player;
    app.radio_dedicated_mode = true;
    assert_scrubbed(&app, "radio mode");
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
    app.update(Msg::Key(key(KeyCode::Char('o'))));
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
fn streaming_mode_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    use crate::streaming::StreamingMode;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Fields: AiEnabled(0), Model(1), ApiKey(2), ReplyLanguage(3), RomanizedTitles(4),
    // Clear cache(5), AutoplayStreaming(6), CuratingMode(7), StreamingMode(8).
    for _ in 0..8 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right))); // Balanced → Discovery
    assert_eq!(
        app.settings.as_ref().unwrap().draft.streaming_mode,
        StreamingMode::Discovery
    );
    assert!(app.status.text.contains("Curating style: Discovery"));
    // Closing settings commits the draft into config + emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.streaming.mode, StreamingMode::Discovery);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn curating_mode_cycles_on_the_ai_tab_and_persists_to_ai_enabled() {
    let _guard = crate::i18n::lock_for_test();
    use crate::streaming::CuratingMode;
    let mut app = app_playing(1, 0);
    assert!(app.config.streaming.ai.enabled); // default → DJ Gem
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    // Down to CuratingMode (index 7), then step it: DJ Gem → YT Native.
    for _ in 0..7 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.curating_mode,
        CuratingMode::YtNative
    );
    assert!(app.status.text.contains("Curating mode:"));
    // Close → the AI rerank flag is now off.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.config.streaming.ai.enabled);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn dj_gem_reply_language_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test(); // English UI
    use crate::i18n::DjGemLanguage;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Down to Reply language (index 3: AiEnabled, Model, ApiKey, ReplyLanguage).
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::DjGemLanguage)
    );
    app.update(Msg::Key(key(KeyCode::Right))); // Auto → English (CYCLE order)
    assert_eq!(
        app.settings.as_ref().unwrap().draft.dj_gem_language,
        DjGemLanguage::English
    );
    assert!(app.status.text.contains("Reply language:"));
    // Closing commits the pick into config and emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.dj_gem_language, DjGemLanguage::English);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn dj_gem_reply_language_is_locked_to_english_under_retro() {
    let _guard = crate::i18n::lock_for_test();
    use crate::i18n::DjGemLanguage;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab)));
    }
    app.settings.as_mut().unwrap().draft.retro_mode = true;
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::DjGemLanguage)
    );
    app.update(Msg::Key(key(KeyCode::Right)));
    // The underlying pick is untouched (still Auto) and the row explains the lock.
    assert_eq!(
        app.settings.as_ref().unwrap().draft.dj_gem_language,
        DjGemLanguage::Auto
    );
    assert!(app.status.text.contains("Retro mode replies in English"));
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .value_display(Field::DjGemLanguage),
        "English (Retro mode)"
    );
}

#[test]
fn streaming_source_cycles_on_general_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    // Fields: Language(0), SearchSource(1), StreamingSource(2).
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Right))); // YouTube -> SoundCloud
    assert_eq!(
        app.settings.as_ref().unwrap().draft.search.streaming_source,
        SearchSource::SoundCloud
    );
    assert!(app.status.text.contains("Streaming source: SoundCloud"));

    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.search.streaming_source, SearchSource::SoundCloud);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn clear_romanized_title_cache_button_is_hidden_in_retro_draft() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
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

    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
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
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ClearRomanizedTitleCache)
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert_eq!(app.status.text, "Romanized title cache cleared");
    assert!(app.romanization.cache.entry_for(&song).is_none());
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::ClearRomanizedTitles)))
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Hotkeys tab (index 2)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
    app.update(Msg::Key(key(KeyCode::Enter))); // capture player.toggle_pause

    // `q` is already Back in Player → a conflict warning pops instead of silently
    // dropping the rebind, and it names the offending chord, action, and context.
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    let conflict = app
        .overlays
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
    assert!(app.overlays.key_conflict.is_none());
    assert!(
        save_config(&cmds).is_none(),
        "dismiss key must be swallowed, not saved"
    );
    assert!(app.settings.is_some(), "settings stayed open after dismiss");
}

/// Move the General-tab cursor onto the Reset-all button.
fn focus_reset_all(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General tab)
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General tab)
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
            crate::keymap::parse_chord("x").unwrap(),
        )
        .unwrap();
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
        Some(Action::TogglePause)
    );

    focus_reset_keybindings(&mut app);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.pending_settings_confirm,
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
        draft_keymap.action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
        None
    );
    // The live keymap follows the existing Settings flow: changes commit on close.
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
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
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
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
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
    // `y` confirms → every draft value is back to its default.
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.overlays.pending_settings_confirm.is_none());
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
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    app.update(Msg::Key(key(KeyCode::Esc))); // anything but Enter/`y` cancels
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
}

#[test]
fn settings_theme_persists_when_closed_with_back() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General)
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..8 {
        // Speed → Seek → Wheel volume → Gapless → Media controls → Auto-continue videos
        // → Video window → EqPreset → Band(0) at row 8.
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..8 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 8
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    for _ in 0..8 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 8
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General); row 0 = language
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General)
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing the key
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
fn editing_api_key_requires_confirmation() {
    // Activating the masked key row clears the buffer, so a stray Enter/click could blank the
    // saved key. Guard it: the first activation asks first; only a confirm enters edit mode.
    let mut app = app_playing(1, 0);
    app.config.gemini_api_key = Some("KEEPME".to_owned());
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row

    // Activation asks first — it does NOT drop straight into edit mode.
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(crate::settings::SettingsConfirm::EditApiKey)
    );
    assert!(!app.settings.as_ref().unwrap().editing_text);

    // Cancelling dismisses the popup and leaves the key untouched (never entered the editor).
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!(!app.settings.as_ref().unwrap().editing_text);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.gemini_api_key,
        "KEEPME"
    );

    // Ask again and confirm (Enter): now edit mode begins with a freshly cleared buffer.
    app.update(Msg::Key(key(KeyCode::Enter))); // request
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!(app.settings.as_ref().unwrap().editing_text);
    assert_eq!(app.settings.as_ref().unwrap().draft.gemini_api_key, "");
}

#[test]
fn api_key_persists_when_leaving_settings_via_close() {
    // The reported bug: type a key, then leave with Esc/q (the intuitive move) — the
    // key must survive.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> buffer cleared
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> masked buffer cleared
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> buffer cleared, key stashed
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft.ai_enabled seeds false)
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

fn streaming_fallback(cmds: &[Cmd]) -> Option<(&str, &str, &[String])> {
    cmds.iter().find_map(|c| match c {
        Cmd::StreamingFallback {
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
    let cmds = app.update(AiMsg::PlayTracks(songs(3)));
    assert_eq!(current(&app), "id0");
    assert!(load_url(&cmds).expect("a Load cmd").contains("id0"));
}

#[test]
fn ai_enqueue_reports_count_and_extends() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue has id0, id1
    app.update(AiMsg::Enqueue(songs(3)));
    assert_eq!(app.queue.len(), 5);
    assert!(app.status.text.contains("Queued"));
}

#[test]
fn ai_error_clears_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.thinking = true;
    app.update(AiMsg::Error("boom".to_owned()));
    assert!(!app.ai.thinking);
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
}

#[test]
fn ai_empty_chat_is_not_appended() {
    let mut app = app_playing(1, 0);
    app.update(AiMsg::Chat("   ".to_owned()));
    assert!(app.ai.messages.is_empty());
    app.update(AiMsg::Chat("here you go".to_owned()));
    assert_eq!(app.ai.messages.len(), 1);
}

#[test]
fn ai_transcript_scrolls_history_and_new_chat_snaps_to_latest() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    for i in 0..30 {
        app.ai.messages.push(AiMessage {
            role: AiRole::Ai,
            text: format!("message {i}"),
        });
    }

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let viewport = app.bridges.ai_transcript_scroll.viewport();
    let content_len = app.bridges.ai_transcript_copy_lines.borrow().len();
    assert!(content_len > viewport, "transcript should overflow");
    assert_eq!(
        app.bridges.ai_transcript_scroll.offset(),
        content_len - viewport,
        "first render should show the newest chat"
    );
    let row = app
        .hits
        .regions()
        .iter()
        .find_map(|b| match b.target {
            MouseTarget::AiTranscriptRow(_) => Some(b.rect),
            _ => None,
        })
        .expect("rendered transcript row");

    app.update(Msg::MouseScroll {
        up: true,
        col: row.x,
        row: row.y,
        ctrl: false,
    });
    assert!(
        app.bridges.ai_transcript_scroll.offset() < content_len - viewport,
        "wheel up should move to older chat"
    );

    app.update(AiMsg::Chat("fresh answer".to_owned()));
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let content_len = app.bridges.ai_transcript_copy_lines.borrow().len();
    assert_eq!(
        app.bridges.ai_transcript_scroll.offset(),
        content_len - app.bridges.ai_transcript_scroll.viewport(),
        "new chat should snap back to the latest line"
    );
}

#[test]
fn dragging_ai_transcript_rows_copies_selection() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "play jazz".to_owned(),
    });
    app.ai.messages.push(AiMessage {
        role: AiRole::Ai,
        text: "queued something mellow".to_owned(),
    });

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let rows: Vec<Rect> = app
        .hits
        .regions()
        .iter()
        .filter_map(|b| match b.target {
            MouseTarget::AiTranscriptRow(_) => Some(b.rect),
            _ => None,
        })
        .collect();
    assert!(rows.len() >= 2, "need at least two transcript rows");

    app.update(Msg::MouseClick {
        col: rows[0].x,
        row: rows[0].y,
    });
    app.update(Msg::MouseDrag {
        col: rows[1].x,
        row: rows[1].y,
    });
    app.update(Msg::MouseLeftUp);

    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(
        app.status.text,
        t!(
            "✓ Chat selection copied to clipboard",
            "✓ 선택한 채팅이 클립보드에 복사됐어요"
        )
    );
}

#[test]
fn ai_submit_button_matches_enter_submit() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.available = true;
    app.ai.input = "play lofi".to_owned();

    let backend = TestBackend::new(80, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let button = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::AiSubmit)
        .map(|b| b.rect)
        .expect("rendered DJ Gem submit button");

    let cmds = app.update(Msg::MouseClick {
        col: button.x,
        row: button.y,
    });
    assert_eq!(ask_ai(&cmds), Some("play lofi"));
    assert!(app.ai.thinking);
    assert!(app.ai.input.is_empty());
}

#[test]
fn ai_suggestion_rows_are_clickable_choices() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "hide onboarding art".to_owned(),
    });
    app.ai.suggestions = songs(4);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row = app
        .hits
        .regions()
        .iter()
        .find_map(|b| match b.target {
            MouseTarget::AiSuggestionRow(2) => Some(b.rect),
            _ => None,
        })
        .expect("rendered DJ Gem suggestion row");

    app.update(Msg::MouseClick {
        col: row.x,
        row: row.y,
    });
    assert_eq!(app.ai.focus, AiFocus::Suggestions);
    assert_eq!(app.ai.suggestions_selected, 2);
}

#[test]
fn ai_streaming_circuit_breaker_disables_after_repeated_empties() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_streaming = true;
    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.update(AiMsg::Enqueue(Vec::new())); // resolves nothing
    }
    assert!(
        !app.autoplay_streaming,
        "streaming disabled after repeated empty extends"
    );
    assert!(app.status.text.contains("Autoplay stopped"));
}

#[test]
fn autoplay_extends_when_queue_runs_low() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.ai.available = true;
    app.autoplay_streaming = true;
    // A manual next advances and should fetch the candidate pool first (both DJ Gem and non-DJ Gem
    // paths share one pool; the DJ Gem reranks it once it returns).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        streaming_fallback(&cmds).is_some(),
        "autoplay should fetch a candidate pool"
    );
    assert!(
        ask_ai(&cmds).is_none(),
        "no free-form DJ Gem streaming prompt anymore"
    );
    assert!(app.streaming.pending);
    assert!(
        !app.ai.thinking,
        "the rerank only starts once the pool returns"
    );
    // The cooldown / in-flight guard blocks an immediate second request.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(streaming_fallback(&cmds).is_none());
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
fn ai_streaming_hands_a_local_shortlist_to_the_reranker() {
    let mut app = app_playing(1, 0); // current id0 is already in history
    let current = app.queue.current().cloned().unwrap();
    app.library
        .record_play(&Song::remote("prev2", "previous two", "artist b", "0:10"));
    app.library
        .record_play(&Song::remote("prev1", "previous one", "artist a", "0:10"));
    app.library.record_play(&current); // current can be present in history; don't duplicate it.
    app.ai.available = true;
    app.autoplay_streaming = true;

    // The fetched pool flows through the local engine; a diverse shortlist goes to the DJ Gem.
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (
                Song::remote("cand1", "Track One", "band one", "3:00"),
                CandidateSource::WatchPlaylist,
            ),
            (
                Song::remote("cand2", "Track Two", "band two", "3:10"),
                CandidateSource::YtdlpStreaming,
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
    assert!(prompt.contains("TASK|streaming_next"));
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
        app.streaming.pending_rerank.is_some(),
        "shortlist + local pick stashed for validation"
    );
    assert!(!app.streaming.pending, "the pool fetch is done");
}

#[test]
fn smart_gate_skips_the_ai_call_and_enqueues_the_local_pick() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0); // current id0, remaining 0 → a refill is due
    app.ai.available = true;
    app.autoplay_streaming = true;
    // smart_gate is on by default; a negative ambiguity gap forces the score-gap branch to read
    // as confident, so this test isolates the gated local path.
    app.config.streaming.ai.smart_gate = true;
    app.config.streaming.ai.ambiguity_gap = -1.0;

    let before = app.queue.len();
    let src = CandidateSource::YtdlpStreaming;
    let cmds = app.update(StreamingMsg::Results {
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
        app.streaming.pending_rerank.is_none(),
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
    app.autoplay_streaming = true;
    // Force the call through the gate so this exercises the cache, not the smart gate.
    app.config.streaming.ai.ambiguity_gap = 1.0;

    let src = CandidateSource::YtdlpStreaming;
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
    let cmds = app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: candidates.clone(),
    });
    assert!(ai_rerank(&cmds).is_some(), "first refill spends a call");
    let pending = app
        .streaming
        .pending_rerank
        .as_ref()
        .expect("rerank stashed");
    let key = pending.cache_key;
    let cached_id = pending.cid_map[0].video_id.clone(); // a real shortlist track

    // Seed the cache as if that rerank had resolved to `cached_id`, then clear the in-flight flags
    // (queue/history untouched → the next identical refill keys to the same entry).
    app.ai_cache_store(key, vec![cached_id.clone()]);
    app.streaming.pending_rerank = None;
    app.ai.thinking = false;
    app.streaming.pending = false;
    app.streaming.last_extend = None;

    // Second identical refill hits the cache → no call; the cached ordering is enqueued directly.
    let cmds = app.update(StreamingMsg::Results {
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
        app.streaming.pending_rerank.is_none(),
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

    let cmds = app.update(AiMsg::SetStationProfile {
        query: "rainy day".to_owned(),
        explore: Some("tight".to_owned()),
        avoid_artists: vec!["Nickelback".to_owned()],
    });

    // The explore level drives the live engine mode, and the profile is stashed for persistence.
    assert_eq!(
        app.config.streaming.mode,
        crate::streaming::StreamingMode::Focused
    );
    assert_eq!(
        app.station.active.as_ref().expect("station stashed").query,
        "rainy day"
    );
    // Both the station and the (now-mode-changed) config are persisted.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );

    // The avoided artist flows into the station state every refill reads.
    let st = app.build_station_state("id0");
    let want = crate::signals::normalize_artist("Nickelback");
    assert!(
        st.banned_artist_keys.contains(&want),
        "avoided artist is banned in refills"
    );
}

#[test]
fn a_plain_start_streaming_without_hints_leaves_no_station() {
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
    app.streaming.feedback_in_flight = true;

    let cmds = app.update(AiMsg::StationPatch {
        down_artists: vec!["Nickelback".to_owned()],
        boost_artists: vec![],
    });

    // The in-flight guard always clears so the next streak can fire again.
    assert!(
        !app.streaming.feedback_in_flight,
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
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile)))
    );
}

#[test]
fn empty_station_patch_clears_inflight_without_persisting() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.station.active = Some(crate::station::StationProfile::from_intent("q", None, &[]));
    app.streaming.feedback_in_flight = true;

    // An empty patch (the off-path summary failed or found nothing) still clears the guard, but a
    // no-op change must not trigger a pointless save.
    let cmds = app.update(AiMsg::StationPatch {
        down_artists: vec![],
        boost_artists: vec![],
    });
    assert!(!app.streaming.feedback_in_flight);
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::StationProfile))),
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
    assert!(app.streaming.feedback_in_flight);
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
    assert!(!app.streaming.feedback_in_flight);
}

#[test]
fn streaming_ai_picks_enqueue_validated_ids_and_top_up_from_local() {
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "S1", "a", "3:00"),
            Song::remote("s2", "S2", "b", "3:00"),
        ],
        local_pick: vec![
            Song::remote("s2", "S2", "b", "3:00"),
            Song::remote("s1", "S1", "a", "3:00"),
        ],
        cid_map: vec![
            crate::streaming::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::streaming::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    // DJ Gem picks one valid cid + one hallucinated cid (dropped); the gap tops up from local.
    app.update(StreamingMsg::AiPicks {
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
    assert!(app.streaming.pending_rerank.is_none(), "pending consumed");
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
fn streaming_ai_picks_for_a_stale_seed_are_ignored() {
    let mut app = app_playing(2, 0);
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "current-seed".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![Song::remote("s1", "S1", "a", "3:00")],
        local_pick: vec![Song::remote("s1", "S1", "a", "3:00")],
        cid_map: vec![crate::streaming::PackedCand {
            cid: "c1".to_owned(),
            video_id: "s1".to_owned(),
        }],
        cache_key: 0,
    });

    // A result for a different (older) seed must not consume the in-flight rerank.
    app.update(StreamingMsg::AiPicks {
        seed_video_id: "old-seed".to_owned(),
        picks: vec![AiPick {
            cid: "c1".to_owned(),
            role: None,
            reasons: vec![],
        }],
        conf: None,
    });
    assert!(
        app.streaming.pending_rerank.is_some(),
        "stale result leaves the current rerank intact"
    );
    assert!(!app.queue.contains_video_id("s1"));
}

#[test]
fn why_ai_overlay_explains_the_last_ai_rerank() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_streaming = true;
    app.ai.thinking = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        mode: crate::streaming::StreamingMode::Balanced,
        shortlist: vec![
            Song::remote("s1", "First Song", "Artist One", "3:00"),
            Song::remote("s2", "Second Song", "Artist Two", "3:00"),
        ],
        local_pick: vec![Song::remote("s2", "Second Song", "Artist Two", "3:00")],
        cid_map: vec![
            crate::streaming::PackedCand {
                cid: "c1".to_owned(),
                video_id: "s1".to_owned(),
            },
            crate::streaming::PackedCand {
                cid: "c2".to_owned(),
                video_id: "s2".to_owned(),
            },
        ],
        cache_key: 0,
    });

    app.update(StreamingMsg::AiPicks {
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
        .streaming
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
    assert!(!app.overlays.why_ai_visible);
    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    assert!(app.radio_dedicated_mode);
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        app.overlays.why_ai_visible,
        "w opens the Why-DJ Gem overlay in Radio mode"
    );
    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(!app.overlays.why_ai_visible, "w again dismisses it");
}

#[test]
fn why_ai_without_a_rerank_shows_a_note_not_an_overlay() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.status.text.clear();
    assert!(app.streaming.last_explain.is_none());

    app.update(Msg::Key(key(KeyCode::Char('w'))));
    assert!(
        !app.overlays.why_ai_visible,
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
    app.streaming.last_explain = Some(StreamingAiExplain {
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
    app.overlays.why_ai_visible = true;

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
fn autoplay_uses_streaming_fallback_without_ai_key() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.autoplay_streaming = true;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        ask_ai(&cmds).is_none(),
        "no Gemini request without an API key"
    );
    let (seed, seed_video_id, exclude_ids) =
        streaming_fallback(&cmds).expect("a fallback streaming command");
    assert_eq!(seed_video_id, "id1");
    assert!(seed.contains("t1"));
    assert!(exclude_ids.iter().any(|id| id == "id0"));
    assert!(exclude_ids.iter().any(|id| id == "id1"));
    assert!(app.streaming.pending);

    let cmds = app.maybe_autoplay_extend();
    assert!(
        streaming_fallback(&cmds).is_none(),
        "pending fallback blocks duplicate requests"
    );
}

#[test]
fn streaming_results_run_through_local_engine_and_clear_pending() {
    let _guard = crate::i18n::lock_for_test();
    fastrand::seed(7);
    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    app.streaming.pending = true;

    // The local engine excludes the seed (id0) and the already-queued track (id1), dedups
    // the repeated id2, and ranks the rest. Distinct artists + normal durations keep the
    // two survivors out of the artist-cooldown / duration hard filters, so both enqueue.
    let src = CandidateSource::YtdlpStreaming;
    app.update(StreamingMsg::Results {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("id0", "current", "a", "3:00"), src), // == seed, dropped
            (Song::remote("id2", "New Song", "c", "3:00"), src), // kept
            (Song::remote("id2", "New Song", "c", "3:00"), src), // canonical duplicate, deduped
            (Song::remote("id1", "queued", "b", "3:00"), src),  // already queued, dropped
            (Song::remote("id3", "Another", "d", "3:00"), src), // kept
        ],
    });

    assert!(!app.streaming.pending, "results clear the in-flight guard");
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
fn streaming_error_uses_circuit_breaker() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.autoplay_streaming = true;

    for _ in 0..AUTOPLAY_MAX_FAILURES {
        app.streaming.pending = true;
        app.update(StreamingMsg::Error {
            seed_video_id: "id0".to_owned(),
            error: "yt-dlp failed".to_owned(),
        });
    }

    assert!(!app.streaming.pending);
    assert!(!app.autoplay_streaming);
    assert!(app.status.text.contains("Autoplay stopped"));
}

#[test]
fn ai_create_and_play_playlist_roundtrip() {
    let mut app = App::new(100);
    let cmds = app.update(AiMsg::CreatePlaylist("Focus".to_owned()));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    let cmds = app.update(AiMsg::AddToPlaylist {
        playlist: "Focus".to_owned(),
        songs: songs(2),
    });
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.playlists.find("Focus").unwrap().songs.len(), 2);
    let cmds = app.update(AiMsg::PlayPlaylist("Focus".to_owned()));
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
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    app.update(Msg::Key(key(KeyCode::Char('f')))); // toggle off
    assert!(!app.library.is_favorite("id0"));
}

#[test]
fn playing_records_history_most_recent_first() {
    let mut app = app_playing(3, 0); // loads id0 -> history [id0]
    app.update(Msg::Key(key(KeyCode::Char('.')))); // id1 -> [id1, id0]
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

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
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

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
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
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
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
    open_library_tab(&mut app, LibraryTab::Downloads);
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

// --- search results-filter popup (`/`) -----------------------------------------

/// Search screen with three results loaded; results arrival focuses the list.
fn app_with_search_results() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![
            fsong("a", "Lovely", "Billie Eilish"),
            fsong("b", "Bad Guy", "Billie Eilish"),
            fsong("c", "Anti-Hero", "Taylor Swift"),
        ],
    });
    assert_eq!(app.search.focus, SearchFocus::Results);
    app
}

fn filter_row_ids(app: &App) -> Vec<String> {
    app.search_filter_rows()
        .iter()
        .map(|(_, s)| s.video_id.clone())
        .collect()
}

#[test]
fn slash_is_bound_to_search_filter() {
    let app = App::new(100);
    let slash = Chord::new(KeyCode::Char('/'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::SearchResults, slash),
        Some(Action::SearchFilter)
    );
}

#[test]
fn slash_opens_the_filter_popup_and_typing_narrows_it() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    // The popup opens fresh: empty query shows the full result set.
    assert_eq!(filter_row_ids(&app), vec!["a", "b", "c"]);

    // Case-insensitive title/artist matching, exactly like the Library filter.
    for c in "BILLIE".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.search_filter.query, "BILLIE");
    assert_eq!(filter_row_ids(&app), vec!["a", "b"]);
    assert_eq!(app.search_filter.cursor, 0);

    // A query with no matches leaves the popup open (still refining); Enter is a no-op.
    for c in " zzz".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert!(filter_row_ids(&app).is_empty());
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert!(app.search_filter.open);
    assert_eq!(app.mode, Mode::Search);

    // Backspace re-widens.
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Backspace)));
    }
    assert_eq!(filter_row_ids(&app), vec!["a", "b"]);
}

#[test]
fn filter_popup_does_not_capture_slash_in_the_input_focus() {
    let mut app = app_with_search_results();
    app.search.focus = SearchFocus::Input;
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    // While the query box is focused, `/` must type, not open the popup.
    assert!(!app.search_filter.open);
    assert_eq!(app.search.input, "/");
}

#[test]
fn filter_popup_enter_plays_the_highlighted_match_and_closes() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "anti".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(filter_row_ids(&app), vec!["c"]);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.search_filter.open);
    // The main cursor lands on the played row (original index), and playback starts.
    assert_eq!(app.search.selected, 2);
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains('c'));
}

#[test]
fn filter_popup_esc_closes_without_acting() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "anti".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(cmds.is_empty());
    assert!(!app.search_filter.open);
    assert!(app.search_filter.query.is_empty());
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.queue.len(), 0);
}

#[test]
fn fresh_results_close_the_filter_popup() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    // A search that was already in flight lands: the rows the popup indexed are gone.
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "y".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![fsong("z", "Zeta", "Nobody")],
    });
    assert!(!app.search_filter.open);
}

#[test]
fn filter_button_opens_the_popup_and_clicking_rows_selects_and_plays() {
    let mut app = app_with_search_results();
    // The `⌕ Filter` button is registered on render and opens the popup.
    click_target(&mut app, MouseTarget::SearchFilterOpen);
    assert!(app.search_filter.open);
    // Single-click a popup row: the popup cursor moves there, nothing plays yet.
    let cmds = click_target(&mut app, MouseTarget::SearchFilterRow(1));
    assert!(cmds.is_empty());
    assert!(app.search_filter.open);
    assert_eq!(app.search_filter.cursor, 1);
    // Double-click plays it and closes the popup, landing the main cursor on the row.
    let cmds = double_click_target(&mut app, MouseTarget::SearchFilterRow(1));
    assert!(!app.search_filter.open);
    assert_eq!(app.search.selected, 1);
    assert_eq!(app.mode, Mode::Player);
    assert!(load_url(&cmds).expect("a Load cmd").contains('b'));
}

#[test]
fn filter_popup_right_click_enqueues_without_closing() {
    // Something is already playing, so the right-click must not interrupt it.
    let mut app = app_playing(2, 0);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![fsong("r0", "R0", "A"), fsong("r1", "R1", "B")],
    });
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::SearchFilterRow(1));
    let before = app.queue.len();
    app.update(Msg::MouseRightClick { col, row });
    // The row got enqueued and the popup stayed open for further picks.
    assert_eq!(app.queue.len(), before + 1);
    assert!(app.search_filter.open);
    assert_eq!(app.search_filter.cursor, 1);
}

#[test]
fn filter_popup_click_outside_closes_it() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    render_app(&app);
    // (0, 0) is the screen corner, well outside the centered popup.
    app.update(Msg::MouseClick { col: 0, row: 0 });
    assert!(!app.search_filter.open);
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn filter_popup_wheel_scrolls_its_own_viewport() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    let songs = (0..40)
        .map(|i| fsong(&format!("id{i}"), &format!("Song {i}"), "A"))
        .collect();
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs,
    });
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    render_app(&app); // records the popup viewport + rect
    let (col, row) = app
        .search_filter
        .rect
        .get()
        .map(|r| (r.x + r.width / 2, r.y + r.height / 2))
        .expect("an open popup rect");
    app.update(Msg::MouseScroll {
        up: false,
        col,
        row,
        ctrl: false,
    });
    assert!(app.search_filter.scroll.offset() > 0);
    // The main results viewport underneath did not move.
    assert_eq!(app.bridges.search_scroll.offset(), 0);
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
        album: None,
        duration_secs: None,
        source: SearchSource::Youtube,
        playable: None,
        local_path: Some(PathBuf::from(path)),
        yt_video_id: None,
    }
}

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
    let root = std::env::temp_dir().join(format!("ytm-tui-app-test-root-{}", std::process::id()));
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
        "ytm-tui-app-test-symlink-root-{}",
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
        app.hits
            .regions()
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
fn library_tabs_mark_favorite_rows_with_a_heart() {
    let song = Song::remote("shared", "Shared Song", "Artist", "0:10");
    let mut app = App::new(100);
    app.library.record_play(&song);
    app.library.toggle_favorite(&song);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let buf = render_app_buffer(&app, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("♥ Shared Song"),
        "favorite rows should show a heart in non-Favorites library tabs too"
    );
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
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.')))); // -> id1
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
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::FetchArtwork { .. })));
    assert!(!app.art.loading);
}

#[test]
fn album_art_on_fetches_remote_then_builds_protocol() {
    let mut app = app_playing(3, 0);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let (resize_tx, _) = tokio::sync::mpsc::channel(8);
    app.set_art_resize_tx(resize_tx);
    // Advancing to id1 now fetches its thumbnail from the remote source.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
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
fn local_deck_linked_track_artwork_still_uses_local_file_source() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    app.art.picker = Some(Picker::halfblocks());
    let mut track = crate::local::LocalTrack::untagged(
        std::path::PathBuf::from("/music/linked-song.m4a"),
        10,
        20,
    );
    track.linked_video_id = Some("abcdefghijk".to_owned());
    let song = track.to_song();

    assert!(song.youtube_id().is_some());
    assert!(matches!(
        app.artwork_source(&song),
        Some(ArtSource::Local(path)) if path.ends_with("linked-song.m4a")
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
    app.update(StreamingMsg::Resolved {
        video_id: "id1".to_owned(),
        stream_url: "https://cdn.example/stream-id1".to_owned(),
    });
    // Skip: id1 should load via the prefetched direct URL, not its watch URL.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    let url = load_url(&cmds).expect("a Load cmd");
    assert_eq!(url, "https://cdn.example/stream-id1");
    // And it should now prefetch id2.
    assert!(resolve_cmd(&cmds, "id2").is_some());
}

#[test]
fn skip_without_prefetch_falls_back_to_watch_url() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.')))); // no Resolved arrived
    let url = load_url(&cmds).expect("a Load cmd");
    assert!(url.contains("music.youtube.com/watch") && url.contains("id1"));
}

// --- M9: mouse controls -------------------------------------------------

#[test]
fn click_on_seekbar_seeks_to_fraction() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
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
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    assert!(app.update(Msg::MouseClick { col: 50, row: 9 }).is_empty()); // wrong row
    assert!(app.update(Msg::MouseClick { col: 200, row: 5 }).is_empty()); // past the bar
}

#[test]
fn click_does_nothing_outside_player_mode() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    app.mode = Mode::Search;
    assert!(app.update(Msg::MouseClick { col: 50, row: 5 }).is_empty());
}

#[test]
fn drag_on_seekbar_scrubs_continuously() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // Press on the bar arms the scrub and seeks (col 25 → 50 s).
    match app.update(Msg::MouseClick { col: 25, row: 5 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 50.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute from the press"),
    }
    // Dragging to a new column seeks continuously — even off the bar's row (row ignored).
    match app.update(Msg::MouseDrag { col: 75, row: 9 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 150.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute from the drag"),
    }
    // Same cell → no duplicate seek (intra-cell dedupe).
    assert!(app.update(Msg::MouseDrag { col: 75, row: 9 }).is_empty());
    // Dragging past the right end pins near the maximum (clamped to width-1, like click-seek).
    match app.update(Msg::MouseDrag { col: 250, row: 5 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 198.0).abs() < 1.0),
        _ => panic!("expected a clamped SeekAbsolute"),
    }
    // Release ends the scrub; a later stray drag does nothing.
    app.update(Msg::MouseLeftUp);
    assert!(
        app.update(Msg::MouseDrag { col: 10, row: 5 }).is_empty(),
        "no scrub after release"
    );
}

#[test]
fn drag_without_a_seekbar_press_does_not_seek() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // No prior press on the bar → a bare drag must not seek.
    assert!(app.update(Msg::MouseDrag { col: 50, row: 5 }).is_empty());
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
        ctrl: false,
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
        ctrl: false,
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
        ctrl: false,
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
    assert!(app.overlays.help_visible);
}

#[test]
fn click_mouse_help_button_opens_mouse_cheatsheet() {
    let mut app = app_playing(1, 0);
    app.register_mouse_button(
        Rect {
            x: 18,
            y: 9,
            width: 8,
            height: 1,
        },
        MouseTarget::MouseHelp,
    );
    assert!(app.update(Msg::MouseClick { col: 20, row: 9 }).is_empty());
    assert!(app.overlays.mouse_help_visible);
    assert!(!app.overlays.help_visible);
}

#[test]
fn korean_q_key_closes_help_overlay() {
    let mut app = app_playing(1, 0);
    app.overlays.help_visible = true;
    assert!(app.update(Msg::Key(key(KeyCode::Char('ㅂ')))).is_empty());
    assert!(!app.overlays.help_visible);
}

#[test]
fn esc_closes_mouse_help_overlay() {
    let mut app = app_playing(1, 0);
    app.overlays.mouse_help_visible = true;
    assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    assert!(!app.overlays.mouse_help_visible);
}

#[test]
fn click_closes_help_overlay_before_buttons() {
    let mut app = app_playing(1, 0);
    app.overlays.help_visible = true;
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
    assert!(!app.overlays.help_visible);
    assert_eq!(app.playback.volume, 40);
}

#[test]
fn click_closes_mouse_help_overlay_before_buttons() {
    let mut app = app_playing(1, 0);
    app.overlays.mouse_help_visible = true;
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
    assert!(!app.overlays.mouse_help_visible);
    assert_eq!(app.playback.volume, 40);
}

fn rendered_help_cluster(app: &App, width: u16, height: u16) -> Rect {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();

    let buttons = app.hits.regions();
    let key = buttons
        .iter()
        .find(|b| b.target == MouseTarget::Global(Action::ToggleHelp))
        .map(|b| b.rect)
        .expect("rendered key help button");
    let mouse = buttons
        .iter()
        .find(|b| b.target == MouseTarget::MouseHelp)
        .map(|b| b.rect)
        .expect("rendered mouse help button");
    let left = key.left().min(mouse.left());
    let top = key.top().min(mouse.top());
    let right = key.right().max(mouse.right());
    let bottom = key.bottom().max(mouse.bottom());
    Rect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
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
        .hits
        .regions()
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
        .hits
        .regions()
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
        .hits
        .regions()
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
    let _guard = crate::i18n::lock_for_test();
    let inner = Rect {
        x: 1,
        y: 1,
        width: 78,
        height: 18,
    };

    let player = App::new(100);
    assert_centered_in(rendered_help_cluster(&player, 80, 20), inner);

    let mut search = App::new(100);
    search.mode = Mode::Search;
    assert_centered_in(rendered_help_cluster(&search, 80, 20), inner);

    let mut library = App::new(100);
    library.mode = Mode::Library;
    assert_centered_in(rendered_help_cluster(&library, 80, 20), inner);

    let mut ai = App::new(100);
    ai.mode = Mode::Ai;
    assert_centered_in(rendered_help_cluster(&ai, 80, 20), inner);
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
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    // Second `f` → dislike; flips the flag and drops the favorite.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(!app.library.is_favorite(&id));
    assert!(app.signals.is_disliked(&id));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
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

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
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
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
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
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    // The skipped track is persisted (SaveSignals) and playback advances.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
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

    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));

    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert_eq!(current(&app), "id0");
}

#[test]
fn eof_records_signals_for_the_finished_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Eof);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
}

#[test]
fn rendering_player_registers_control_buttons() {
    let app = app_playing(2, 0);
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buttons = app.hits.regions();
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
    assert!(buttons.iter().any(|b| b.target == MouseTarget::MouseHelp));
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
    assert!(app.hits.seekbar_rect().is_some());
}

#[test]
fn rendering_settings_registers_clickable_controls() {
    // Each control kind must publish its own hit target *on top of* the row-select rect, so a
    // click changes/activates the value rather than only moving the cursor onto it.
    let render_targets = |tab: SettingsTab| -> Vec<MouseTarget> {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (mode → Settings)
        app.settings.as_mut().unwrap().tab = tab;
        let backend = TestBackend::new(80, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
        app.hits.regions().iter().map(|b| b.target).collect()
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
        app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
        app.settings.as_mut().unwrap().tab = tab;
        let backend = TestBackend::new(80, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
        let rect = app
            .hits
            .regions()
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

    let buttons = app.hits.regions();
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
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // A click on the seekbar with the dropdown open just closes it (no seek emitted).
    let cmds = app.update(Msg::MouseClick { col: 50, row: 5 });
    assert!(!app.dropdowns.eq_open);
    assert!(cmds.is_empty());
}

#[test]
fn art_overlay_mask_tracks_each_popup_independently() {
    use super::artwork::*;

    // The render loop clears native terminal graphics on any change to this mask, so every
    // art-covering surface needs its own bit — switching one straight to another, or stacking a
    // second over a first, must register as an edge.
    let mut app = app_playing(1, 0);
    assert_eq!(app.art_overlay_mask(), 0);
    app.dropdowns.eq_open = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_EQ_BIT);
    // Switch eq -> streaming: the mask still changes even though some popup
    // stays open across the switch.
    app.dropdowns.eq_open = false;
    app.dropdowns.streaming_open = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_STREAMING_BIT);
    // The queue window is a distinct bit, and can stack with a dropdown.
    app.queue_popup.open = true;
    assert_eq!(
        app.art_overlay_mask(),
        ART_OVERLAY_STREAMING_BIT | ART_OVERLAY_QUEUE_BIT
    );
    app.dropdowns.streaming_open = false;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_QUEUE_BIT);
    app.queue_popup.open = false;
    assert_eq!(app.art_overlay_mask(), 0);

    app.overlays.help_visible = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_HELP_BIT);
    app.overlays.help_visible = false;
    app.overlays.about_visible = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_ABOUT_BIT);
    app.overlays.about_visible = false;
    app.overlays.why_ai_visible = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_WHY_AI_BIT);
    app.overlays.why_ai_visible = false;
    app.overlays.key_conflict = Some(Conflict {
        ctx: KeyContext::Player,
        existing: Action::TogglePause,
        chord: Chord::new(KeyCode::Char('x'), KeyModifiers::NONE),
    });
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_KEY_CONFLICT_BIT);
    app.overlays.key_conflict = None;
    app.radio_mode.pending_radio_mode_confirm = Some(RadioModeConfirm::Enter);
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_RADIO_CONFIRM_BIT);
    app.radio_mode.pending_radio_mode_confirm = None;
    app.overlays.pending_settings_confirm = Some(SettingsConfirm::ResetAll);
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_SETTINGS_CONFIRM_BIT);
    app.overlays.pending_settings_confirm = None;
    app.library_ui.confirm_delete = Some(vec![std::path::PathBuf::from("track.mp3")]);
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_LIBRARY_CONFIRM_BIT);
    app.library_ui.confirm_delete = None;
    // The bulk-download confirm deliberately shares bit 9 with the file-delete confirm: the two
    // Library confirm modals are mutually exclusive (each captures all keys while open) and share
    // the same footprint, so one bit tracks both without a missed graphics-clear edge.
    app.library_ui.confirm_download = Some(vec![fsong("z", "Z", "A")]);
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_LIBRARY_CONFIRM_BIT);
    app.library_ui.confirm_download = None;
    app.mode = Mode::Search;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_NOT_PLAYER_BIT);
    app.mode = Mode::Player;
    assert_eq!(app.art_overlay_mask(), 0);
    app.overlays.mouse_help_visible = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_MOUSE_HELP_BIT);
    app.overlays.mouse_help_visible = false;
    assert_eq!(app.art_overlay_mask(), 0);
    app.library_ui.create_input = Some("New list".to_owned());
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_CREATE_PLAYLIST_BIT);
    app.library_ui.create_input = None;
    app.library_ui.confirm_playlist_delete = Some("mix".to_owned());
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_DELETE_PLAYLIST_BIT);
    app.library_ui.confirm_playlist_delete = None;
    app.playlist_picker = Some(PlaylistPicker {
        songs: vec![fsong("pick", "Pick", "Artist")],
        cursor: 0,
        naming: None,
    });
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_PLAYLIST_PICKER_BIT);
    app.playlist_picker = None;
    assert_eq!(app.art_overlay_mask(), 0);
    app.search_filter.open = true;
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_SEARCH_FILTER_BIT);
    app.search_filter.open = false;
    assert_eq!(app.art_overlay_mask(), 0);
}

fn configure_test_art_picker(app: &mut App, protocol: ratatui_image::picker::ProtocolType) {
    let mut picker = ratatui_image::picker::Picker::halfblocks();
    picker.set_protocol_type(protocol);
    app.config.album_art = Some(true);
    app.art.picker = Some(picker);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
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
    fn set_streaming(app: &mut App, open: bool) {
        app.dropdowns.streaming_open = open;
    }
    fn set_queue(app: &mut App, open: bool) {
        app.queue_popup.open = open;
    }
    fn set_about(app: &mut App, open: bool) {
        app.overlays.about_visible = open;
    }
    fn set_why_ai(app: &mut App, open: bool) {
        app.overlays.why_ai_visible = open;
    }

    for (name, set_open) in [
        ("eq dropdown", set_eq as fn(&mut App, bool)),
        ("streaming dropdown", set_streaming),
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

    app.overlays.about_visible = true;
    app.update(Msg::Resize);
    assert!(!app.take_clear_before_draw());
}

#[test]
fn art_overlay_transition_does_not_clear_for_halfblocks_art() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);

    app.overlays.about_visible = true;
    app.update(Msg::Resize);
    assert!(!app.take_clear_before_draw());
}

#[test]
fn about_native_icon_transition_requests_clear_without_album_art() {
    let mut app = app_playing(1, 0);
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Sixel);

    app.overlays.about_visible = true;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw());
    assert!(!app.take_clear_before_draw());

    app.overlays.about_visible = false;
    app.update(Msg::Resize);
    assert!(app.take_clear_before_draw());
}

#[test]
fn artwork_arriving_under_overlay_requests_full_clear() {
    let mut app = app_playing(1, 0);
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Sixel);
    app.overlays.about_visible = true;
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
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
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
    fn set_streaming(app: &mut App, open: bool) {
        app.dropdowns.streaming_open = open;
    }
    fn set_help(app: &mut App, open: bool) {
        app.overlays.help_visible = open;
    }
    fn set_mouse_help(app: &mut App, open: bool) {
        app.overlays.mouse_help_visible = open;
    }
    fn set_about(app: &mut App, open: bool) {
        app.overlays.about_visible = open;
    }

    for (name, set_open) in [
        ("eq dropdown", set_eq as fn(&mut App, bool)),
        ("streaming dropdown", set_streaming),
        ("help overlay", set_help),
        ("mouse help overlay", set_mouse_help),
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
    let _guard = crate::i18n::lock_for_test();
    let player_area = ratatui::layout::Rect::new(0, 0, 80, 20);
    let modal_area = ratatui::layout::Rect::new(0, 0, 80, 24);

    let mut eq = app_playing(3, 0);
    eq.dropdowns.eq_open = true;
    let buf = render_app_buffer(&eq, player_area.width, player_area.height);
    assert_opaque_rect(
        &buf,
        dropdown_popup_rect(&eq, |t| matches!(t, MouseTarget::EqSelect(_))),
    );

    let mut streaming = app_playing(3, 0);
    streaming.autoplay_streaming = true;
    streaming.dropdowns.streaming_open = true;
    let buf = render_app_buffer(&streaming, player_area.width, player_area.height);
    assert_opaque_rect(
        &buf,
        dropdown_popup_rect(&streaming, |t| matches!(t, MouseTarget::StreamingSelect(_))),
    );

    let mut queue = app_playing(5, 0);
    queue.open_queue_popup();
    let buf = render_app_buffer(&queue, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, queue.queue_popup.rect.get().unwrap());

    let mut help = app_playing(1, 0);
    help.overlays.help_visible = true;
    let buf = render_app_buffer(&help, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_percent(modal_area, 80, 80));

    let mut mouse_help = app_playing(1, 0);
    mouse_help.overlays.mouse_help_visible = true;
    let buf = render_app_buffer(&mouse_help, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_percent(modal_area, 84, 82));

    let mut about = app_playing(1, 0);
    about.overlays.about_visible = true;
    let buf = render_app_buffer(&about, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 60, 22));

    let mut why = app_playing(2, 0);
    why.streaming.last_explain = Some(StreamingAiExplain {
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
    why.overlays.why_ai_visible = true;
    let buf = render_app_buffer(&why, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 72, 9));

    let mut conflict = app_playing(1, 0);
    conflict.overlays.key_conflict = Some(Conflict {
        ctx: KeyContext::Player,
        existing: Action::TogglePause,
        chord: Chord::new(KeyCode::Char('x'), KeyModifiers::NONE),
    });
    let buf = render_app_buffer(&conflict, modal_area.width, modal_area.height);
    assert_opaque_rect(&buf, centered_fixed(modal_area, 54, 9));

    let mut reset = app_playing(1, 0);
    reset.overlays.pending_settings_confirm = Some(SettingsConfirm::ResetAll);
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
    app.overlays.about_visible = true;
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
    app.overlays.about_visible = true;

    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    app.art.picker = Some(picker);

    let buf = render_app_buffer(&app, area.width, area.height);
    let cached_protocol = app
        .overlays
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
    app.overlays.about_visible = true;

    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Sixel);
    app.art.picker = Some(picker);

    let buf = render_app_buffer(&app, area.width, area.height);
    let cached_protocol = app
        .overlays
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
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
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
    app.overlays.about_visible = true;
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
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
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
    app.overlays.about_visible = true;
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
    app.overlays.about_visible = true;

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
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
fn rendering_player_registers_streaming_menu_when_autoplay_on() {
    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|b| b.target == MouseTarget::StreamingMenu)
    );
}

#[test]
fn streaming_dropdown_renders_mode_rows_when_open() {
    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    app.dropdowns.streaming_open = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buttons = app.hits.regions();
    for mode in crate::streaming::StreamingMode::CYCLE {
        assert!(
            buttons
                .iter()
                .any(|b| b.target == MouseTarget::StreamingSelect(mode)),
            "missing dropdown row for {mode:?}"
        );
    }
}

#[test]
fn clicking_streaming_label_closes_eq_and_opens_streaming_dropdown() {
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
        MouseTarget::StreamingMenu,
    );
    assert!(app.update(Msg::MouseClick { col: 42, row: 4 }).is_empty());
    assert!(app.dropdowns.streaming_open);
    assert!(!app.dropdowns.eq_open);
}

#[test]
fn selecting_streaming_mode_applies_and_persists() {
    use crate::streaming::StreamingMode;
    let mut app = app_playing(1, 0);
    app.dropdowns.streaming_open = true;
    app.register_mouse_button(
        Rect {
            x: 40,
            y: 6,
            width: 9,
            height: 1,
        },
        MouseTarget::StreamingSelect(StreamingMode::Discovery),
    );
    let cmds = app.update(Msg::MouseClick { col: 43, row: 6 });
    assert_eq!(app.config.streaming.mode, StreamingMode::Discovery);
    assert!(!app.dropdowns.streaming_open);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
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

fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
    (0..buf.area.height).any(|y| buffer_row(buf, y).contains(needle))
}

#[test]
fn now_playing_overlay_render_registers_state_specific_actions() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = radio_card("Artist - Track");
    app.ai.available = true;

    let buf = render_app_buffer(&app, 80, 24);

    assert!(buffer_contains(&buf, "Now playing"));
    assert!(buffer_contains(&buf, "Track"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::NowPlayingFavorite)
    );
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::NowPlayingAskAi)
    );
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::CloseNowPlaying)
    );

    let mut no_metadata = radio_playing("quiet");
    no_metadata.update(Msg::Key(key(KeyCode::Char('i'))));
    let buf = render_app_buffer(&no_metadata, 80, 24);
    assert!(buffer_contains(&buf, "doesn't expose song info"));
    assert!(
        no_metadata
            .hits
            .regions()
            .iter()
            .all(|r| r.target != MouseTarget::NowPlayingFavorite
                && r.target != MouseTarget::NowPlayingAskAi)
    );
}

#[test]
fn settings_modal_renders_expose_actionable_hit_targets() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Settings;
    app.open_settings();

    app.overlays.key_conflict = Some(crate::keymap::Conflict {
        ctx: KeyContext::Player,
        existing: Action::NextTrack,
        chord: crate::keymap::Chord::from(key(KeyCode::Char('n'))),
    });
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Keybinding conflict"));
    assert!(buffer_contains(&buf, "Next track"));

    app.overlays.key_conflict = None;
    app.overlays.pending_settings_confirm = Some(SettingsConfirm::ResetAll);
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Confirm reset all settings"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::ConfirmSettings)
    );
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::CancelSettings)
    );

    app.overlays.pending_settings_confirm = None;
    app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker {
        selected: 1,
        items: vec![
            crate::transfer::actor::PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyLiked,
                label: "Liked Songs".to_owned(),
                total: 25,
            },
            crate::transfer::actor::PickerPlaylist {
                source: crate::transfer::TransferSource::SpotifyPlaylist {
                    id: "pl-1".to_owned(),
                },
                label: "Roadtrip".to_owned(),
                total: 7,
            },
        ],
    });
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Import from Spotify"));
    assert!(buffer_contains(&buf, "Roadtrip"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::SpotifyPickRow(1))
    );
}

#[test]
fn recording_popups_render_rows_and_controls() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Settings;
    app.open_settings();
    app.overlays.recording_settings = Some(RecordingSettingsPopup::default());

    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Radio recording"));
    assert!(buffer_contains(&buf, "Min duration"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::RecordingRow(0))
    );
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| matches!(r.target, MouseTarget::RecordingSlider(1)))
    );

    app.overlays.recordings_browser = Some(RecordingsBrowser::default());
    app.recorder
        .history
        .push_back(crate::recorder::RecordedTrack {
            id: 1,
            title: Some("Track".to_owned()),
            artist: Some("Artist".to_owned()),
            raw: String::new(),
            station: Some("Station".to_owned()),
            temp_path: PathBuf::from("/tmp/rec-1.mp3"),
            ext: "mp3",
            duration_secs: 181,
            state: crate::recorder::RecordingState::Recorded,
            final_path: None,
        });
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Radio recordings"));
    assert!(buffer_contains(&buf, "Artist - Track"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::RecordingBrowseRow(0))
    );
    assert!(buffer_contains(&buf, "s save"));
    assert!(buffer_contains(&buf, "d discard"));
}

#[test]
fn library_playlist_popups_render_create_picker_and_confirmations() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Library;

    app.library_ui.create_input = Some("New Mix".to_owned());
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "New playlist"));
    assert!(buffer_contains(&buf, "New Mix"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::ConfirmPlaylistCreate)
    );

    app.library_ui.create_input = None;
    let playlist_id = app.playlists.create("Roadtrip").unwrap();
    app.playlists
        .add(&playlist_id, Song::remote("a", "A", "Artist", "1:00"));
    app.playlist_picker = Some(PlaylistPicker {
        songs: vec![Song::remote("b", "B", "Artist", "2:00")],
        cursor: 0,
        naming: None,
    });
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Add to playlist"));
    assert!(buffer_contains(&buf, "Roadtrip"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::PlaylistPickRow(0))
    );

    app.playlist_picker = None;
    app.library_ui.confirm_playlist_delete = Some(playlist_id.clone());
    let buf = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buf, "Delete playlist"));
    assert!(buffer_contains(&buf, "Roadtrip"));
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::ConfirmPlaylistDelete)
    );
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
        .hits
        .regions()
        .iter()
        .filter_map(|b| is_row(b.target).then_some(b.rect))
        .collect();
    assert!(
        !rects.is_empty(),
        "dropdown row rects were not registered; targets: {:?}",
        app.hits
            .regions()
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
    app.hits
        .regions()
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
        let buttons = app.hits.regions();
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
fn right_clicking_a_queue_row_removes_that_track() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));

    let cmds = app.update(Msg::MouseRightClick { col, row });

    assert!(load_url(&cmds).is_none());
    assert_eq!(app.queue.len(), 4);
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id2"),
        "the right-clicked track should be gone from the queue"
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

// --- Library Playlists tab ------------------------------------------------

/// Opens the Playlists tab with two playlists ("Alpha": a1+a2, "Beta": b1).
fn app_with_playlists() -> App {
    let mut app = App::new(100);
    app.playlists.create("Alpha");
    app.playlists.add("Alpha", fsong("a1", "Song A1", "X"));
    app.playlists.add("Alpha", fsong("a2", "Song A2", "Y"));
    app.playlists.create("Beta");
    app.playlists.add("Beta", fsong("b1", "Song B1", "Z"));
    open_library_tab(&mut app, LibraryTab::Playlists);
    app
}

#[test]
fn playlists_is_the_last_normal_tab() {
    assert_eq!(
        LibraryTab::NORMAL,
        [
            LibraryTab::All,
            LibraryTab::Favorites,
            LibraryTab::History,
            LibraryTab::Downloads,
            LibraryTab::Playlists,
        ]
    );
}

#[test]
fn n_is_bound_to_playlist_create_in_the_playlists_context() {
    let app = App::new(100);
    let n = Chord::new(KeyCode::Char('n'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Playlists, n),
        Some(Action::PlaylistCreate)
    );
    // The song tabs must not gain the binding.
    assert_eq!(app.keymap.action(KeyContext::Library, n), None);
}

#[test]
fn playlists_root_lists_playlists() {
    let app = app_with_playlists();
    assert!(app.playlists_root());
    assert_eq!(app.library_len(), 2);
    assert_eq!(app.library_count_for(LibraryTab::Playlists), 2);
    // Root rows are playlists, so there are no song rows to act on.
    assert!(app.library_rows().is_empty());
}

#[test]
fn enter_opens_a_playlist_and_back_returns_to_the_list() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("alpha"));
    assert_eq!(row_ids(&app), vec!["a1", "a2"]);

    app.update(Msg::Key(key(KeyCode::Char('q')))); // back to the playlist list
    assert_eq!(app.mode, Mode::Library);
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.selected, 0); // cursor restored to "Alpha"

    app.update(Msg::Key(key(KeyCode::Char('q')))); // and out of the Library
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn esc_backs_out_of_an_opened_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.open_playlist.is_some());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.mode, Mode::Library);
}

#[test]
fn a_on_the_playlist_root_plays_that_playlist_as_a_fresh_queue() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Down))); // cursor to "Beta"
    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "b1");
    assert!(load_url(&cmds).is_some());
}

#[test]
fn enter_in_the_drilldown_plays_the_selected_song() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    app.update(Msg::Key(key(KeyCode::Down))); // cursor to a2
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "a2");
    assert!(load_url(&cmds).is_some());
}

#[test]
fn delete_on_the_playlist_root_asks_then_deletes_on_confirm() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Delete)));
    // Nothing deleted yet — the modal is pending.
    assert_eq!(
        app.library_ui.confirm_playlist_delete.as_deref(),
        Some("alpha")
    );
    assert_eq!(app.playlists.list().len(), 2);

    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.confirm_playlist_delete.is_none());
    assert_eq!(app.playlists.list().len(), 1);
    assert!(app.playlists.find("alpha").is_none());
    assert!(app.status.text.contains("Deleted playlist"));
}

#[test]
fn any_other_key_cancels_the_playlist_delete_confirm() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library_ui.confirm_playlist_delete.is_some());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.confirm_playlist_delete.is_none());
    assert_eq!(app.playlists.list().len(), 2);
}

#[test]
fn delete_in_the_drilldown_removes_the_song_from_the_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    let cmds = app.update(Msg::Key(key(KeyCode::Delete))); // remove a1, no confirm
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("alpha"));
    assert_eq!(row_ids(&app), vec!["a2"]);
    assert_eq!(app.playlists.find("alpha").unwrap().songs.len(), 1);
}

#[test]
fn n_opens_the_create_popup_and_enter_creates_and_selects() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert_eq!(app.library_ui.create_input.as_deref(), Some(""));

    for c in "My Mix".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_ui.create_input.as_deref(), Some("My Mix"));

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.create_input.is_none());
    assert!(app.playlists.find("My Mix").is_some());
    // The cursor lands on the new playlist (appended last).
    assert_eq!(app.library_ui.selected, 2);
    assert!(app.status.text.contains("Created playlist"));
}

#[test]
fn esc_cancels_the_create_popup_without_creating() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.create_input.is_none());
    assert_eq!(app.playlists.list().len(), 2);
}

#[test]
fn blank_create_popup_enter_hints_instead_of_creating() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    // The popup stays open for a correction.
    assert!(app.library_ui.create_input.is_some());
    assert_eq!(app.playlists.list().len(), 2);
    assert!(app.status.text.contains("Enter a playlist name"));
}

#[test]
fn slash_filters_playlist_names_at_the_root() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "be".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.filtered_playlists()[0].name, "Beta");

    // Enter commits the filter; the next Enter opens the (only) filtered playlist.
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("beta"));
}

#[test]
fn switching_tabs_closes_an_opened_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.open_playlist.is_some());
    app.update(Msg::Key(key(KeyCode::Tab))); // wraps to All
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.tab, LibraryTab::All);
}

#[test]
fn backslash_on_the_playlist_root_enqueues_the_whole_playlist() {
    // Playing state, so the enqueue is a pure append (idle enqueues start playback).
    let mut app = app_playing(1, 0);
    app.playlists.create("Alpha");
    app.playlists.add("Alpha", fsong("a1", "Song A1", "X"));
    app.playlists.add("Alpha", fsong("a2", "Song A2", "Y"));
    open_library_tab(&mut app, LibraryTab::Playlists);
    let before = app.queue.len();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert!(load_url(&cmds).is_none()); // no interruption
    assert_eq!(app.queue.len(), before + 2); // both "Alpha" tracks
    assert_eq!(app.mode, Mode::Library); // enqueue never leaves the screen
}

#[test]
fn playlists_reload_reconciles_a_dangling_drilldown() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    assert!(app.library_ui.open_playlist.is_some());
    // Simulate an external rewrite (finished transfer job) that dropped "alpha".
    app.playlists = crate::playlists::Playlists::default();
    app.reconcile_playlists_reload();
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.selected, 0);
}

// --- "Add to playlist" picker (p / P) --------------------------------------

/// Favorites tab with two tracks and one existing playlist ("Mix").
fn app_with_picker_fixture() -> App {
    let mut app = app_with_favorites(vec![
        fsong("s1", "Song One", "A"),
        fsong("s2", "Song Two", "B"),
    ]);
    app.playlists.create("Mix");
    app
}

#[test]
fn p_is_bound_to_add_to_playlist_and_listed_in_the_cheat_sheet() {
    let app = App::new(100);
    let p = Chord::new(KeyCode::Char('p'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Library, p),
        Some(Action::AddToPlaylist)
    );
    assert_eq!(
        app.keymap.action(KeyContext::SearchResults, p),
        Some(Action::AddToPlaylist)
    );
    let shift_p = Chord::new(KeyCode::Char('P'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Player, shift_p),
        Some(Action::AddToPlaylist)
    );
}

#[test]
fn p_on_a_library_row_opens_the_picker_and_enter_adds() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs.len(), 1);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.playlist_picker.is_none());
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 1);
    assert!(app.status.text.contains("Added 1 track to Mix"));
}

#[test]
fn p_with_a_multiselect_range_adds_the_whole_range() {
    let mut app = app_with_picker_fixture();
    app.library_ui.selected = 0;
    app.library_ui.anchor = 1;
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert_eq!(app.playlist_picker.as_ref().unwrap().songs.len(), 2);
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 2);
}

#[test]
fn adding_a_duplicate_reports_already_there_without_saving() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // first add
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter))); // same song again
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 1);
    assert!(app.status.text.contains("Already in playlist"));
}

#[test]
fn picker_n_creates_a_playlist_and_adds_in_one_go() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Char('n')))); // jump to the name entry
    assert!(app.playlist_picker.as_ref().unwrap().naming.is_some());
    for c in "Road".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.playlist_picker.is_none());
    assert_eq!(app.playlists.find("Road").unwrap().songs.len(), 1);
}

#[test]
fn picker_esc_backs_out_of_naming_then_closes() {
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    app.update(Msg::Key(key(KeyCode::Esc)));
    let picker = app.playlist_picker.as_ref().expect("back to the list");
    assert!(picker.naming.is_none());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.playlist_picker.is_none());
    assert!(app.playlists.find("Mix").unwrap().songs.is_empty()); // nothing added
}

#[test]
fn shift_p_on_the_player_picks_up_the_current_track() {
    let mut app = app_playing(2, 0);
    app.playlists.create("Mix");
    app.update(Msg::Key(key(KeyCode::Char('P'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs[0].video_id, "id0");
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs[0].video_id, "id0");
    assert_eq!(app.mode, Mode::Player); // adding never leaves the screen
}

#[test]
fn p_on_a_search_result_picks_that_result() {
    let mut app = App::new(100);
    app.playlists.create("Mix");
    app.mode = Mode::Search;
    app.search.results = songs(2);
    app.search.focus = SearchFocus::Results;
    app.search.selected = 1;
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs[0].video_id, "id1");
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs[0].video_id, "id1");
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn p_is_a_noop_on_the_playlists_root() {
    let mut app = app_with_playlists();
    assert!(app.playlists_root());
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert!(app.playlist_picker.is_none());
}

#[test]
fn entering_the_playlists_tab_nudges_playlist_creation() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    open_library_tab(&mut app, LibraryTab::Playlists);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "Press n to create a new playlist");
}

#[test]
fn the_playlist_create_nudge_follows_a_rebind() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.keymap
        .rebind(
            KeyContext::Playlists,
            Action::PlaylistCreate,
            crate::keymap::parse_chord("b").unwrap(),
        )
        .unwrap();
    open_library_tab(&mut app, LibraryTab::Playlists);
    assert_eq!(app.status.text, "Press b to create a new playlist");
}

#[test]
fn mouse_tab_click_on_playlists_also_nudges() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.on_mouse_target(MouseTarget::LibraryTab(LibraryTab::Playlists));
    assert_eq!(app.library_ui.tab, LibraryTab::Playlists);
    assert!(app.status.text.contains("create a new playlist"));
}

// Text zoom (Ctrl+wheel / Ctrl+-/=) ------------------------------------------------

#[test]
fn zoom_keys_step_the_scale_and_persist_it() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(app.zoom.percent(), 125, "first step is the fine 25% notch");
    assert_eq!(app.zoom.scale(), 2, "125% already renders on the 2x grid");
    assert_eq!(app.config.text_zoom, Some(125));
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(cfg))] if cfg.text_zoom == Some(125)
    ));
    assert!(app.status.text.contains("125%"));

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('-'))));
    assert_eq!(app.zoom.percent(), 100);
    assert_eq!(app.zoom.scale(), 1);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(cfg))] if cfg.text_zoom == Some(100)
    ));
}

#[test]
fn zoom_clamps_at_both_ends_with_a_toast_and_no_save() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.zoom.set(300);
    app.config.text_zoom = Some(300);

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(app.zoom.percent(), 300);
    assert!(cmds.is_empty(), "at max: no config churn, just a toast");
    assert!(!app.status.text.is_empty());

    app.zoom.set(100);
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('-'))));
    assert_eq!(app.zoom.percent(), 100);
    assert!(cmds.is_empty());
}

#[test]
fn zoom_on_unsupported_terminal_explains_itself_and_stays_at_1() {
    let mut app = app_playing(1, 0);
    assert!(!app.zoom.supported(), "mode defaults to unsupported");

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(app.zoom.scale(), 1);
    assert!(cmds.is_empty());
    assert!(
        !app.status.text.is_empty(),
        "a cheat-sheet-advertised key must never be silently dead"
    );
}

#[test]
fn ctrl_wheel_steps_zoom_instead_of_scrolling() {
    let mut app = app_playing(3, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);

    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: 5,
        row: 5,
        ctrl: true,
    });
    assert_eq!(app.zoom.scale(), 2);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(_))]
    ));

    let cmds = app.update(Msg::MouseScroll {
        up: false,
        col: 5,
        row: 5,
        ctrl: true,
    });
    assert_eq!(app.zoom.scale(), 1);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(_))]
    ));
}

#[test]
fn zoom_keys_work_while_the_help_overlay_is_open() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.overlays.help_visible = true;

    app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(
        app.zoom.scale(),
        2,
        "the cheat-sheet advertises Ctrl+=; it must work while the sheet is open"
    );
    assert!(
        app.overlays.help_visible,
        "zoom must not dismiss the overlay"
    );
}

#[test]
fn zoom_change_forces_a_full_clear_redraw() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert!(
        app.take_clear_before_draw(),
        "stale multicells from the old grid must be cleared"
    );
}

#[test]
fn persisted_zoom_is_restored_only_when_the_terminal_supports_it() {
    let cfg = Config {
        text_zoom: Some(150),
        ..Config::default()
    };

    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(
        app.zoom.percent(),
        100,
        "unsupported terminal ignores the persisted level"
    );

    let mut app = App::new(100);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.apply_config(&cfg);
    assert_eq!(app.zoom.percent(), 150);
    assert_eq!(app.zoom.scale(), 2);
}

#[test]
fn native_album_art_is_hidden_while_zoomed() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.config.album_art = Some(true);
    app.art.picker = Some(ratatui_image::picker::Picker::halfblocks());
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.set_art_resize_tx(tx);
    let img = image::DynamicImage::new_rgb8(4, 4);
    app.set_artwork("vid0".to_owned(), Some(img));
    assert!(app.art_active(), "sanity: art shows at scale 1");

    app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    // The halfblocks picker renders art as text cells, which keep scaling.
    assert!(app.art_active(), "halfblock art is text and survives zoom");

    app.art
        .picker
        .as_mut()
        .unwrap()
        .set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
    assert!(
        !app.art_active(),
        "pixel-protocol art would stripe across scaled rows; it must hide"
    );
    app.update(Msg::Key(ctrl(KeyCode::Char('-'))));
    assert!(app.art_active(), "art returns at scale 1");
}

#[test]
fn retro_render_scrubs_cjk_metadata_without_unsupported_cells() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    let cfg = Config {
        retro_mode: true,
        ..Config::default()
    };
    app.apply_config(&cfg);
    app.queue.set(
        vec![Song::remote("cjk", "한글 제목 日本語", "가수 简体", "3:00")],
        0,
    );
    app.load_song(app.queue.current().cloned());

    let buf = render_app_buffer(&app, 80, 24);

    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).expect("cell inside buffer");
            assert!(
                crate::ui::retro::retro_supported(cell.symbol()),
                "unsupported retro cell at ({x},{y}): {:?}",
                cell.symbol()
            );
        }
    }
}

// Responsive layout under zoom / narrow grids ---------------------------------------

/// Render the UI into a TestBackend of the given size and return the registered mouse
/// targets plus the first screen row as text (the nav strip).
fn render_at(app: &App, w: u16, h: u16) -> (Vec<MouseTarget>, String) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let top: String = (0..w)
        .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
        .collect::<Vec<_>>()
        .join("");
    let targets = app.hits.regions().iter().map(|b| b.target).collect();
    (targets, top)
}

#[test]
fn narrow_nav_pages_with_arrows_so_every_screen_stays_clickable() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.mode = Mode::Library;

    let (targets, top) = render_at(&app, 40, 12);
    assert!(top.contains('◀') && top.contains('▶'), "paged nav: {top:?}");
    assert!(
        top.contains("Library"),
        "the active tab must stay visible: {top:?}"
    );
    // The arrows page to the neighbor screens even though their tabs may be off-screen.
    assert!(targets.contains(&MouseTarget::Nav(Mode::Search)));
    assert!(targets.contains(&MouseTarget::Nav(Mode::Settings)));

    // Wide grids keep the full strip — no arrows.
    let (_, top) = render_at(&app, 100, 30);
    assert!(!top.contains('◀'), "full-width nav must not page: {top:?}");
    assert!(top.contains("DJ Gem"));
}

#[test]
fn narrow_nav_arrows_navigate_on_click() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.mode = Mode::Library;
    render_at(&app, 40, 12);

    // Find the ◀ arrow's registered rect (the Nav(Search) target) and click it.
    let rect = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .map(|b| b.rect)
        .expect("left arrow registered");
    app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
    });
    assert_eq!(app.mode, Mode::Search, "◀ pages to the previous screen");
}

#[test]
fn korean_nav_hitbox_still_matches_zoomed_mouse_cells() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);

    crate::i18n::set_language(crate::i18n::Language::Korean);
    let (_, top) = render_at(&app, 100, 24);
    assert!(
        top.contains("검 색"),
        "Korean nav label should render: {top:?}"
    );
    let rect = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .map(|b| b.rect)
        .expect("Korean Search tab registered");
    crate::i18n::set_language(crate::i18n::Language::English);

    let virtual_col = rect.x + rect.width / 2;
    let virtual_row = rect.y;
    let mut input = crate::event::Translator::default();
    let msg = input
        .translate(
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: virtual_col * 2 + 1,
                row: virtual_row * 2 + 1,
                modifiers: KeyModifiers::NONE,
            }),
            2,
            2,
        )
        .expect("mouse press translated");

    app.update(msg);

    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn nav_arrows_hollow_at_first_and_last_tab() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);

    // First tab (Player): can't go left — the left arrow renders hollow.
    let (_, top) = render_at(&app, 40, 12);
    assert!(
        top.contains('◁') && !top.contains('◀'),
        "first tab: left arrow should be hollow: {top:?}"
    );
    assert!(top.contains('▶'), "right arrow stays filled: {top:?}");

    // Last tab (DJ Gem): can't go right — the right arrow renders hollow.
    app.mode = Mode::Ai;
    let (_, top) = render_at(&app, 40, 12);
    assert!(
        top.contains('▷') && !top.contains('▶'),
        "last tab: right arrow should be hollow: {top:?}"
    );
    assert!(top.contains('◀'), "left arrow stays filled: {top:?}");
}

#[test]
fn ai_model_label_yields_to_nav_when_narrow() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    // Pin the model so the label under test is stable (the reported artifact was the
    // "Latest" label surviving as a clipped "est" after the nav's ▶ arrow).
    app.ai.model = crate::ai::GeminiModel::Latest;

    // Wide: the model label rides the right end of the border row, as before.
    let (_, top) = render_at(&app, 100, 30);
    assert!(
        top.contains("Latest"),
        "wide: the model label should show: {top:?}"
    );

    // Narrow: the nav strip needs the row — the label disappears entirely instead of
    // leaving a clipped tail (this used to render as a stray \"est\" after the ▶ arrow).
    let (_, top) = render_at(&app, 30, 30);
    assert!(
        !top.contains("Latest") && !top.contains("est"),
        "narrow: no label and no clipped remnant: {top:?}"
    );
}

#[test]
fn search_rows_keep_alignment_when_selection_scrolls_off() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(40);
    app.search.selected = 0;

    // Column of each row's leading `[` source tag.
    let bracket_cols = |buf: &ratatui::buffer::Buffer| -> Vec<usize> {
        (0..24)
            .filter_map(|y| buffer_row(buf, y).chars().position(|c| c == '['))
            .collect()
    };

    let before = render_app_buffer(&app, 80, 24);
    let cols_before = bracket_cols(&before);
    assert!(!cols_before.is_empty(), "results should be on screen");

    // Wheel far past the keyboard selection (which stays on row 0, now off-screen).
    for _ in 0..10 {
        app.update(Msg::MouseScroll {
            up: false,
            col: 40,
            row: 12,
            ctrl: false,
        });
    }
    let after = render_app_buffer(&app, 80, 24);
    let cols_after = bracket_cols(&after);
    assert!(
        app.bridges.search_scroll.offset() > 0,
        "the wheel must actually scroll the viewport"
    );
    assert_eq!(
        cols_before[0], cols_after[0],
        "rows must not shift left when the selection leaves the viewport (the ▶ gutter is reserved unconditionally)"
    );
    assert!(
        cols_after.iter().all(|&c| c == cols_after[0]),
        "every visible row shares one tag column: {cols_after:?}"
    );
}

#[test]
fn search_hearts_reserve_a_fixed_slot() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(2);
    let fav = app.search.results[0].clone();
    app.library.toggle_favorite(&fav);

    let buf = render_app_buffer(&app, 80, 24);
    let title_col = |needle: &str| -> usize {
        (0..24)
            .map(|y| buffer_row(&buf, y))
            .find_map(|row| row.find(needle).map(|byte| row[..byte].chars().count()))
            .unwrap_or_else(|| panic!("row containing {needle:?}"))
    };
    assert_eq!(
        title_col("t0"),
        title_col("t1"),
        "favorited and plain rows must start their titles in the same column"
    );
}

#[test]
fn selected_row_marquee_scrolls_with_animations_off() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = vec![
        Song::remote(
            "long",
            "An Extremely Long Title That Cannot Possibly Fit In A Narrow Terminal Row",
            "Some Artist",
            "3:00",
        ),
        Song::remote("short", "Tiny", "A", "0:10"),
    ];
    app.search.selected = 0;
    assert!(
        !app.config.animations.master,
        "every animation toggle is off"
    );

    app.anim.anim_frame = 0;
    let first = render_app_buffer(&app, 40, 15);
    assert!(
        app.animation_active(),
        "a clipped selected row must keep the clock awake with the masters off"
    );

    let long_y = (1..15)
        .find(|&y| buffer_row(&first, y).contains("Extremely"))
        .expect("selected row");
    let short_y = (1..15)
        .find(|&y| buffer_row(&first, y).contains("Tiny"))
        .expect("neighbor row");

    app.anim.anim_frame = 60;
    let later = render_app_buffer(&app, 40, 15);
    assert_ne!(
        buffer_row(&first, long_y),
        buffer_row(&later, long_y),
        "the clipped selected row crawls so its whole text can be read"
    );
    assert_eq!(
        buffer_row(&first, short_y),
        buffer_row(&later, short_y),
        "non-selected rows stay byte-identical"
    );

    // Selecting a row that fits lets the clock sleep again.
    app.search.selected = 1;
    let _ = render_app_buffer(&app, 40, 15);
    assert!(
        !app.animation_active(),
        "a fitting selected row must not keep the clock awake"
    );
}

#[test]
fn help_overlay_scrolls_with_wheel_and_keys_and_resets_on_open() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert!(app.overlays.help_visible);

    // First render records the viewport; the sheet is far longer than 12 rows.
    render_at(&app, 40, 12);
    assert_eq!(app.bridges.help_scroll.offset(), 0);

    app.update(Msg::MouseScroll {
        up: false,
        col: 5,
        row: 5,
        ctrl: false,
    });
    let after_wheel = app.bridges.help_scroll.offset();
    assert!(after_wheel > 0, "wheel-down must scroll the sheet");

    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.bridges.help_scroll.offset(), after_wheel + 1);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.bridges.help_scroll.offset(), after_wheel);
    assert!(
        app.overlays.help_visible,
        "scroll keys must not dismiss the overlay"
    );

    // Close and reopen: the offset starts back at the top.
    app.update(Msg::Key(key(KeyCode::Esc)));
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.bridges.help_scroll.offset(), 0);
}

#[test]
fn decdhl_terminals_step_straight_to_double_size() {
    let mut app = app_playing(1, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Decdhl);

    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(
        app.zoom.percent(),
        200,
        "no intermediate levels without OSC 66"
    );
    assert_eq!(app.zoom.scale(), 2);
    assert!(app.status.text.contains("200%"));
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(cfg))] if cfg.text_zoom == Some(200)
    ));

    // At the top the toast quotes this mode's real maximum, not the OSC 66 ladder's.
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert!(cmds.is_empty());
    assert!(app.status.text.contains("200%"), "{}", app.status.text);

    app.update(Msg::Key(ctrl(KeyCode::Char('-'))));
    assert_eq!(app.zoom.percent(), 100);
}

#[test]
fn zoom_wheel_lock_freezes_the_gesture_but_not_the_keys() {
    let mut app = app_playing(3, 0);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);

    // Ctrl+L locks, persists, and explains itself.
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('l'))));
    assert_eq!(app.config.zoom_wheel_lock, Some(true));
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(_))]
    ));
    assert!(!app.status.text.is_empty());

    // Ctrl+wheel now scrolls instead of zooming.
    app.update(Msg::MouseScroll {
        up: true,
        col: 5,
        row: 5,
        ctrl: true,
    });
    assert_eq!(app.zoom.percent(), 100, "locked gesture must not zoom");

    // The keyboard chords stay live.
    app.update(Msg::Key(ctrl(KeyCode::Char('='))));
    assert_eq!(app.zoom.percent(), 125);

    // Ctrl+L again unlocks and the wheel zooms once more.
    app.update(Msg::Key(ctrl(KeyCode::Char('l'))));
    assert_eq!(app.config.zoom_wheel_lock, Some(false));
    app.update(Msg::MouseScroll {
        up: true,
        col: 5,
        row: 5,
        ctrl: true,
    });
    assert_eq!(app.zoom.percent(), 150);
}

// --- Video overlay: auto-continue (`Settings › Playback`) ----------------

use crate::player::video::VideoEvent;

/// A live stand-in for the overlay mpv process, so `on_video_overlay_event`'s
/// "window still open" guard holds during a test. Killed via `close_video()`.
#[cfg(unix)]
fn fake_overlay_proc() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test child")
}

#[test]
fn video_continue_advances_queue_paused_and_loads_next_video() {
    let mut app = app_playing(3, 0);
    // The overlay paused the audio when it opened.
    app.playback.paused = true;
    app.video.paused_audio = true;

    let cmds = app.video_continue_next();

    assert_eq!(current(&app), "id1");
    // The next track loads into the audio engine (position tracking)…
    assert!(load_url(&cmds).is_some());
    // …but both sides stay pinned paused: video owns playback until the overlay closes.
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Player(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(true)
    )));
    // The same overlay window is asked to show the next track's video.
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
}

#[test]
fn video_continue_at_queue_end_stops_like_audio() {
    let mut app = app_playing(2, 1);
    app.playback.paused = true;
    app.video.paused_audio = true;

    let cmds = app.video_continue_next();

    // Mirrors the audio queue-end: nothing left loaded, mpv told to drop the file.
    assert!(has_stop(&cmds));
    assert!(!cmds.iter().any(|c| matches!(c, Cmd::VideoLoad(_))));
    assert!(app.prefetch.loaded_video_id.is_none());
    // Nothing to resume when the (already closed) overlay state is cleaned up.
    assert!(!app.video.paused_audio);
    assert!(app.playback.paused);
}

#[test]
fn video_continue_repeat_one_reloads_the_same_video() {
    let mut app = app_playing(2, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    app.playback.paused = true;
    app.video.paused_audio = true;

    let cmds = app.video_continue_next();

    assert_eq!(current(&app), "id0");
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id0"
    )));
}

#[test]
fn video_event_after_close_is_ignored() {
    let mut app = app_playing(2, 0);
    app.config.auto_continue_videos = Some(true);
    // The overlay was already closed (`v`): a late Eof from its IPC client is stale.
    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });
    assert!(cmds.is_empty());
    assert_eq!(current(&app), "id0");
}

#[cfg(unix)]
#[test]
fn video_eof_with_toggle_off_closes_and_resumes_audio() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });

    // Toggle off: an ended video reads as a close — window reaped, audio resumed.
    assert!(app.video.proc.is_none());
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Player(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(false)
    )));
    assert_eq!(current(&app), "id0", "no advance with the toggle off");
}

#[cfg(unix)]
#[test]
fn video_eof_with_toggle_on_keeps_the_window_and_advances() {
    let mut app = app_playing(3, 0);
    app.config.auto_continue_videos = Some(true);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Eof,
    });

    assert!(
        app.video.proc.is_some(),
        "the window stays open for the next video"
    );
    assert_eq!(current(&app), "id1");
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
    app.close_video();
}

#[cfg(unix)]
#[test]
fn video_event_from_an_older_generation_is_ignored() {
    let mut app = app_playing(2, 0);
    app.config.auto_continue_videos = Some(true);
    app.video.proc = Some(fake_overlay_proc());
    app.video.generation = 3;

    // A Quit from the window that Shift+V already replaced must not close the new one.
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation: 2,
        event: VideoEvent::Quit,
    });

    assert!(cmds.is_empty());
    assert!(app.video.proc.is_some());
    app.close_video();
}

#[cfg(unix)]
#[test]
fn video_next_key_skips_and_shows_the_next_video() {
    let mut app = app_playing(3, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Next,
    });

    assert_eq!(current(&app), "id1");
    // Audio stays pinned paused under the video; the window shows the landed track.
    assert!(app.playback.paused && app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id1"
    )));
    app.close_video();
}

#[cfg(unix)]
#[test]
fn video_prev_key_goes_back_a_video() {
    let mut app = app_playing(3, 1);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;

    let generation = app.video.generation;
    let cmds = app.update(PlayerMsg::VideoOverlay {
        generation,
        event: VideoEvent::Prev,
    });

    assert_eq!(current(&app), "id0");
    assert!(app.playback.paused && app.video.paused_audio);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::VideoLoad(url) if url == "https://www.youtube.com/watch?v=id0"
    )));
    app.close_video();
}

// --- Playlist search & import (`Ctrl+P` kind, ytpl: rows) -----------------

#[test]
fn ctrl_p_toggles_playlist_search_kind_and_routes_submit() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // Search screen, input focus
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Playlists);

    for c in "study".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SearchPlaylists { query, .. } if query == "study")),
        "playlist kind must route to the playlist search command"
    );
    assert!(app.search.searching);

    // Toggling back restores ordinary source-routed search.
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Songs);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Search { .. })));
}

/// A search-results screen with one playlist row selected.
fn app_with_playlist_row() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = vec![Song::remote(
        "ytpl:PLabcdefgh1234",
        "Rainy Mix",
        "Curator",
        "12 tracks",
    )];
    app.search.selected = 0;
    app
}

#[test]
fn enter_on_a_playlist_row_fetches_tracks_to_play() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks { playlist_id, intent: crate::api::PlaylistIntent::Play, .. }
            if playlist_id == "PLabcdefgh1234"
    )));
}

#[test]
fn enqueue_and_import_keys_map_to_their_playlist_intents() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks {
            intent: crate::api::PlaylistIntent::Enqueue,
            ..
        }
    )));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks {
            intent: crate::api::PlaylistIntent::Import,
            ..
        }
    )));
}

#[test]
fn playlist_tracks_play_replaces_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    let cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Play,
        songs: songs(3),
    });
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id0");
    assert!(load_url(&cmds).is_some());
    assert!(app.status.text.contains("Rainy Mix"));
}

#[test]
fn playlist_tracks_enqueue_appends_to_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Enqueue,
        songs: vec![Song::remote("zzz11111111", "t", "a", "0:10")],
    });
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id0", "current track is untouched");
}

#[test]
fn playlist_tracks_import_creates_a_local_playlist() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    let cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Import,
        songs: songs(3),
    });
    let imported = app
        .playlists
        .playlists
        .iter()
        .find(|p| p.name == "Rainy Mix")
        .expect("imported playlist");
    assert_eq!(imported.songs.len(), 3);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
}

#[test]
fn empty_playlist_fetch_reports_instead_of_wiping_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    let cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Play,
        songs: Vec::new(),
    });
    assert!(cmds.is_empty());
    assert_eq!(app.queue.len(), 2, "queue survives an empty fetch");
    assert!(!app.status.text.is_empty());
}
