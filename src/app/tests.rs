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
    app.update(Msg::Key(key(KeyCode::Char('/')))); // open search (input focused)
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
    app.update(Msg::Key(key(KeyCode::Char('/'))));
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
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.search.select_all);
    app.update(Msg::Key(ctrl(KeyCode::Char('h')))); // go home
    assert!(!app.search.select_all, "highlight must not survive leaving the screen");

    // AI box: same story — select all, leave, flag cleared so it can't reappear highlighted.
    app.update(Msg::Key(key(KeyCode::Char('a')))); // enter AI
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
    app.update(Msg::Key(key(KeyCode::Char('a')))); // open AI assistant (input focused)
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
    assert!(load_url(&cmds).is_some(), "should resume by loading the new track");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("b"));
}

#[test]
fn radio_extend_prefetches_next_while_playing() {
    let mut app = App::new(100);
    app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
    app.prefetch.loaded_video_id = Some("a".to_owned()); // still playing the seed
    let cmds = app.extend_queue_from_radio(vec![Song::remote("b", "B", "y", "2:00")]);
    assert!(load_url(&cmds).is_none(), "must not interrupt the playing track");
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
fn slash_enters_search_and_q_is_typed_not_quit() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert_eq!(app.mode, Mode::Search);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.should_quit);
    assert_eq!(app.search.input, "q");
}

#[test]
fn korean_letters_still_type_in_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
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
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(!app.should_scrub_ime_preedit());
}

#[test]
fn enter_in_search_emits_search_cmd() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [Cmd::Search(q)] => assert_eq!(q, "lofi"),
        _ => panic!("expected a Search cmd"),
    }
}

#[test]
fn search_submit_stays_enter_when_common_confirm_is_remapped() {
    let mut app = App::new(100);
    app.keymap = confirm_on_f5_keymap();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert!(cmds.is_empty());
    assert!(!app.search.searching);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [Cmd::Search(q)] => assert_eq!(q, "lofi"),
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
        [Cmd::Search(q)] => assert_eq!(q, "lofi"),
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
fn enter_on_search_result_appends_without_wiping_the_playing_queue() {
    // A 3-track queue is already playing track 0.
    let mut app = app_playing(3, 0);
    let before_len = app.queue.len();
    let playing = app.prefetch.loaded_video_id.clone();
    assert_eq!(playing.as_deref(), Some("id0"));

    // Go to search, pick a fresh result, hit Enter.
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        query: "x".to_owned(),
        songs: vec![Song::remote("new9", "New", "Z", "3:00")],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 0;
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

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
    app.update(Msg::Key(key(KeyCode::Char('/'))));
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
fn r_cycles_repeat_and_s_toggles_shuffle() {
    let mut app = app_playing(3, 0);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
    assert!(!app.queue.shuffle);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert!(app.queue.shuffle);
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
    app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_radio);
    // Plain `r` still cycles repeat (not the autoplay toggle).
    app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(app.autoplay_radio);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
    app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_radio);
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
    assert!(app.autoplay_radio);
}

#[test]
fn seek_keys_use_the_configured_interval() {
    let mut app = app_playing(1, 0);
    app.apply_config(&crate::config::Config { seek_seconds: Some(30.0), ..Default::default() });
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
fn transient_status_expires_after_ttl_and_restores_the_title() {
    let mut app = app_playing(1, 0);
    // A notification covers the title and arms the expiry timer.
    app.update(Msg::Key(key(KeyCode::Char('N')))); // toggle normalize → sets status
    assert!(!app.status.text.is_empty(), "an action should set a status");
    assert!(app.status_visible(), "a non-empty status arms the expiry tick");

    // Before the TTL elapses, a tick is a no-op — the notification stays.
    app.update(Msg::StatusTick);
    assert!(!app.status.text.is_empty(), "status persists until the TTL elapses");
    assert!(app.status_visible());

    // Backdate the timer past the TTL; the next tick clears it and restores the title.
    app.status.set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    app.dirty = false; // so the assertion below proves the clear requested the redraw
    app.update(Msg::StatusTick);
    assert!(app.status.text.is_empty(), "status auto-clears after the TTL");
    assert!(!app.status_visible(), "expiry disarms the tick");
    assert!(app.dirty, "clearing the status requests a redraw of the title");
}

#[test]
fn radio_mode_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    use crate::radio::RadioMode;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Fields: AiEnabled(0), Model(1), ApiKey(2), AutoplayRadio(3), RadioMode(4).
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right))); // Balanced → Discovery
    assert_eq!(app.settings.as_ref().unwrap().draft.radio_mode, RadioMode::Discovery);
    assert!(app.status.text.contains("Radio mode: Discovery"));
    // Closing settings commits the draft into config + emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.radio.mode, RadioMode::Discovery);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveConfig(_))));
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
    let conflict = app.key_conflict.expect("a conflict warning should be raised");
    assert_eq!(conflict.existing, Action::Back);
    assert_eq!(conflict.ctx, KeyContext::Player);
    assert_eq!(conflict.chord, crate::keymap::parse_chord("q").unwrap());
    // The binding was left untouched: space still toggles pause, `q` still means Back.
    let km = &app.settings.as_ref().unwrap().keymap;
    assert_eq!(
        km.action(KeyContext::Player, crate::keymap::parse_chord("space").unwrap()),
        Some(Action::TogglePause)
    );
    assert_eq!(
        km.action(KeyContext::Player, crate::keymap::parse_chord("q").unwrap()),
        Some(Action::Back)
    );

    // The popup is modal: the next key only dismisses it (here `q` does NOT save+quit).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(app.key_conflict.is_none());
    assert!(save_config(&cmds).is_none(), "dismiss key must be swallowed, not saved");
    assert!(app.settings.is_some(), "settings stayed open after dismiss");
}

/// Move the General-tab cursor onto the Reset-all button.
fn focus_reset_all(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings (General tab)
    for _ in 0..SettingsTab::General.fields().len() - 1 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(app.settings.as_ref().unwrap().current_field(), Some(Field::ResetAll));
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
    assert!(app.confirm_reset_all);
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
    // `y` confirms → every draft value is back to its default.
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(!app.confirm_reset_all);
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
    assert!(app.confirm_reset_all);
    app.update(Msg::Key(key(KeyCode::Esc))); // anything but Enter/`y` cancels
    assert!(!app.confirm_reset_all);
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
        // ThemeColor rows start at field index 2 (after ThemePreset and BackgroundNone).
        st.row = 2 + crate::theme::ThemeRole::ALL
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
    assert!((app.playback.speed - 1.1).abs() < 1e-9, "speed applied on close");
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
    assert!((app.playback.speed - 1.1).abs() < 1e-9, "speed persisted on close");
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
    for _ in 0..4 {
        // Speed → Seek → Gapless → EqPreset → Band(0) at row 4.
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
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 4
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
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Down))); // → Band(0) at row 4
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
    app.update(Msg::Key(key(KeyCode::Down))); // row 1 = cookies file
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
        "committing a changed key must reload the AI actor"
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
        app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab (index 4)
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
    assert_eq!(save_config(&cmds).unwrap().gemini_api_key.as_deref(), Some("KEEPME"));
}

#[test]
fn reset_all_re_enables_ai() {
    // Reset All must restore *every* field to its default, including the AI on/off switch —
    // otherwise a user who disabled AI then reset would be stranded with AI off.
    let mut app = app_playing(1, 0);
    app.config.ai_enabled = Some(false);
    app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft.ai_enabled seeds false)
    assert!(!app.settings.as_ref().unwrap().draft.ai_enabled);
    app.settings_reset_all();
    assert!(
        app.settings.as_ref().unwrap().draft.ai_enabled,
        "reset returns AI to its default (enabled)"
    );
}

// --- A: AI assistant ----------------------------------------------------

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
        } => Some((seed.as_str(), seed_video_id.as_str(), exclude_ids.as_slice())),
        _ => None,
    })
}

/// The `(seed_video_id, prompt)` of the `AiRerank` command among `cmds`, if any.
fn ai_rerank(cmds: &[Cmd]) -> Option<(&str, &str)> {
    cmds.iter().find_map(|c| match c {
        Cmd::AiRerank { seed_video_id, prompt } => {
            Some((seed_video_id.as_str(), prompt.as_str()))
        }
        _ => None,
    })
}

#[test]
fn a_enters_ai_from_player_and_library() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(app.mode, Mode::Ai);
    assert_eq!(app.ai.focus, AiFocus::Input);
    // And from the library view.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(app.mode, Mode::Ai);
}

#[test]
fn ai_submit_without_key_shows_onboarding_error() {
    let mut app = app_playing(1, 0); // ai_available defaults to false
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    for c in "play jazz".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(ask_ai(&cmds).is_none(), "no AskAi without a key");
    assert!(!app.ai.thinking);
    // Transcript holds the user prompt plus an error line.
    assert_eq!(app.ai.messages.last().unwrap().role, AiRole::Error);
    assert!(
        app.ai.messages
            .iter()
            .any(|m| m.role == AiRole::User && m.text == "play jazz")
    );
}

#[test]
fn ai_submit_with_key_emits_ask_and_sets_thinking() {
    let mut app = app_playing(1, 0);
    app.ai.available = true;
    app.update(Msg::Key(key(KeyCode::Char('a'))));
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
    // A manual next advances and should fetch the candidate pool first (both AI and non-AI
    // paths share one pool; the AI reranks it once it returns).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(
        radio_fallback(&cmds).is_some(),
        "autoplay should fetch a candidate pool"
    );
    assert!(ask_ai(&cmds).is_none(), "no free-form AI radio prompt anymore");
    assert!(app.radio.pending);
    assert!(!app.ai.thinking, "the rerank only starts once the pool returns");
    // The cooldown / in-flight guard blocks an immediate second request.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(radio_fallback(&cmds).is_none());
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

    // The fetched pool flows through the local engine; a diverse shortlist goes to the AI.
    let src = CandidateSource::YtdlpRadio;
    let cmds = app.update(Msg::RadioResults {
        seed_video_id: "id0".to_owned(),
        candidates: vec![
            (Song::remote("cand1", "Track One", "band one", "3:00"), src),
            (Song::remote("cand2", "Track Two", "band two", "3:10"), src),
            (Song::remote("cand3", "Track Three", "band three", "3:20"), src),
        ],
    });

    let (seed_id, prompt) = ai_rerank(&cmds).expect("an AI rerank command");
    assert_eq!(seed_id, "id0");
    // Session context (current + the two previous tracks).
    assert!(prompt.contains("- Current: t0 — a"));
    assert!(prompt.contains("- Previous 1: previous one — artist a"));
    assert!(prompt.contains("- Previous 2: previous two — artist b"));
    // The exact candidate ids the model may choose from.
    assert!(prompt.contains("cand1"));
    assert!(prompt.contains("cand2"));
    assert!(app.ai.thinking, "the rerank is in flight");
    assert!(app.radio.pending_rerank.is_some(), "shortlist + local pick stashed for validation");
    assert!(!app.radio.pending, "the pool fetch is done");
}

#[test]
fn radio_ai_picks_enqueue_validated_ids_and_top_up_from_local() {
    let mut app = app_playing(2, 0); // queue id0 (current), id1
    app.ai.available = true;
    app.autoplay_radio = true;
    app.ai.thinking = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "id0".to_owned(),
        shortlist: vec![
            Song::remote("s1", "S1", "a", "3:00"),
            Song::remote("s2", "S2", "b", "3:00"),
        ],
        local_pick: vec![
            Song::remote("s2", "S2", "b", "3:00"),
            Song::remote("s1", "S1", "a", "3:00"),
        ],
    });

    // AI returns one valid id + one hallucinated id (dropped); the gap tops up from local.
    app.update(Msg::RadioAiPicks {
        seed_video_id: "id0".to_owned(),
        ids: vec!["s1".to_owned(), "HALLUCINATED".to_owned()],
    });

    assert!(!app.ai.thinking, "rerank finished");
    assert!(app.radio.pending_rerank.is_none(), "pending consumed");
    assert!(app.queue.contains_video_id("s1"), "valid AI id enqueued");
    assert!(app.queue.contains_video_id("s2"), "topped up from local pick");
    assert!(!app.queue.contains_video_id("HALLUCINATED"), "hallucinated id dropped");
}

#[test]
fn radio_ai_picks_for_a_stale_seed_are_ignored() {
    let mut app = app_playing(2, 0);
    app.ai.available = true;
    app.autoplay_radio = true;
    app.ai.thinking = true;
    app.radio.pending_rerank = Some(PendingRerank {
        seed_video_id: "current-seed".to_owned(),
        shortlist: vec![Song::remote("s1", "S1", "a", "3:00")],
        local_pick: vec![Song::remote("s1", "S1", "a", "3:00")],
    });

    // A result for a different (older) seed must not consume the in-flight rerank.
    app.update(Msg::RadioAiPicks {
        seed_video_id: "old-seed".to_owned(),
        ids: vec!["s1".to_owned()],
    });
    assert!(app.radio.pending_rerank.is_some(), "stale result leaves the current rerank intact");
    assert!(!app.queue.contains_video_id("s1"));
}

#[test]
fn autoplay_uses_radio_fallback_without_ai_key() {
    let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
    app.autoplay_radio = true;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(ask_ai(&cmds).is_none(), "no Gemini request without an API key");
    let (seed, seed_video_id, exclude_ids) = radio_fallback(&cmds).expect("a fallback radio command");
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
    assert_eq!(app.queue.len(), 4, "two new tracks added to the queue of two");
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
    let mut app = app_playing(3, 0);
    app.keymap = confirm_on_f5_keymap();
    app.library.toggle_favorite(&songs(2)[1]);
    app.library.toggle_favorite(&songs(2)[0]);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(key(KeyCode::Down)));

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(load_url(&cmds).is_none());
    assert_eq!(app.mode, Mode::Library);

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
    app.library.toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.library.toggle_favorite(&Song::remote("b", "tb", "x", "0:10"));
    app.library.toggle_favorite(&Song::remote("c", "tc", "x", "0:10")); // [c, b, a]
    open_library_tab(&mut app, LibraryTab::Favorites);
    // Cursor on row 0, drag-anchor on row 1: the selection spans rows 0..=1 (c, b).
    app.library_ui.selected = 0;
    app.library_ui.anchor = 1;
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    let ids: Vec<&str> = app.library.favorites.iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, vec!["a"]);
    assert_eq!(app.library_ui.selected, 0);
}

#[test]
fn library_delete_range_removes_from_history() {
    let mut app = App::new(100);
    app.library.record_play(&Song::remote("a", "ta", "x", "0:10"));
    app.library.record_play(&Song::remote("b", "tb", "x", "0:10"));
    app.library.record_play(&Song::remote("c", "tc", "x", "0:10")); // front->back: c, b, a
    open_library_tab(&mut app, LibraryTab::History);
    app.library_ui.selected = 1;
    app.library_ui.anchor = 2; // rows 1..=2 = b, a
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    let ids: Vec<&str> = app.library.history.iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, vec!["c"]);
}

#[test]
fn library_page_and_jump_keys_move_the_cursor() {
    let mut app = App::new(100);
    for i in 0..30 {
        app.library.record_play(&Song::remote(format!("id{i}"), format!("t{i}"), "x", "0:10"));
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
        app.library.record_play(&Song::remote(format!("id{i}"), format!("t{i}"), "x", "0:10"));
    }
    open_library_tab(&mut app, LibraryTab::History);
    app.library_ui.selected = 0;
    let len = app.library_len();
    app.bridges.library_scroll.resolve(app.library_ui.selected, 10, len, SCROLLOFF);

    app.update(Msg::MouseScroll { up: false });
    assert_eq!(app.library_ui.selected, 0); // selection untouched by the wheel
    assert_eq!(app.bridges.library_scroll.resolve(app.library_ui.selected, 10, len, SCROLLOFF), 3);
    app.update(Msg::MouseScroll { up: true });
    assert_eq!(app.bridges.library_scroll.resolve(app.library_ui.selected, 10, len, SCROLLOFF), 0); // clamped at top

    // Search: same decoupling, clamped at the last page.
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(20);
    app.search.selected = 19;
    let len = app.search.results.len();
    app.bridges.search_scroll.resolve(app.search.selected, 10, len, SCROLLOFF); // offset -> last page (10)
    app.update(Msg::MouseScroll { up: false });
    assert_eq!(app.search.selected, 19); // selection untouched
    assert_eq!(app.bridges.search_scroll.resolve(app.search.selected, 10, len, SCROLLOFF), 10); // clamped at end
    app.update(Msg::MouseScroll { up: true });
    assert_eq!(app.bridges.search_scroll.resolve(app.search.selected, 10, len, SCROLLOFF), 7);
}

#[test]
fn library_delete_is_disabled_in_all_tab() {
    let mut app = App::new(100);
    app.library.toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.is_empty());
    assert_eq!(app.library.favorites.len(), 1); // untouched
}

#[test]
fn library_all_dedups_same_title_across_collections() {
    let mut app = App::new(100);
    app.library.toggle_favorite(&Song::remote("yt1", "Song", "Artist", "3:00"));
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
    app.library.toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.library.toggle_favorite(&Song::remote("b", "tb", "x", "0:10"));
    app.library.toggle_favorite(&Song::remote("c", "tc", "x", "0:10")); // [c, b, a]
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;

    // Render so the per-row hit rects are published.
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row_rect = |i: usize| {
        app.bridges.mouse_buttons
            .borrow()
            .iter()
            .find(|b| b.target == MouseTarget::ListRow(i))
            .map(|b| b.rect)
            .expect("a rendered library row rect")
    };
    let r0 = row_rect(0);
    let r2 = row_rect(2);

    // Click row 0 (anchors the range), then drag onto row 2 (extends it).
    app.update(Msg::MouseClick { col: r0.x, row: r0.y });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (0, 0));
    app.update(Msg::MouseDrag { col: r2.x, row: r2.y });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (2, 0));

    // Delete removes the whole selected 0..=2 range.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library.favorites.is_empty());
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
    assert!(app.lyrics.track.as_ref().is_some_and(|l| l.lines.len() == 2));
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
    assert!(matches!(app.artwork_source(&song), Some(ArtSource::Local(_))));
}

#[test]
fn art_fit_rect_centers_by_aspect() {
    let mut app = App::new(100);
    app.art.picker = Some(Picker::halfblocks()); // font cell 10x20 px
    app.art.dims = (100, 100); // square source
    let r = app.art_fit_rect(Rect { x: 0, y: 0, width: 40, height: 40 });
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
    assert_eq!(app.downloads.active.get("id0"), Some(&DownloadState::Running(0)));
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
    assert_eq!(app.downloads.active.get("id0"), Some(&DownloadState::Running(43)));
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
    assert_eq!(app.downloads.active.get("id0"), Some(&DownloadState::Failed));
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

    app.bridges.mouse_buttons
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
        overflow.library.record_play(&Song::remote(format!("id{i}"), format!("t{i}"), "x", "0:10"));
    }
    overflow.mode = Mode::Library;
    overflow.library_ui.tab = LibraryTab::History;
    assert!(has_thumb(&overflow), "a long list should show a scrollbar thumb");

    let mut fits = App::new(100);
    fits.library.record_play(&Song::remote("a", "ta", "x", "0:10"));
    fits.library.record_play(&Song::remote("b", "tb", "x", "0:10"));
    fits.mode = Mode::Library;
    fits.library_ui.tab = LibraryTab::History;
    assert!(!has_thumb(&fits), "a short list should not show a scrollbar");
}

fn assert_centered_in(rect: Rect, container: Rect) {
    let left = rect.x.saturating_sub(container.x);
    let right = container
        .x
        .saturating_add(container.width)
        .saturating_sub(rect.x.saturating_add(rect.width));
    assert_eq!(left, right, "help button should be centered in {container:?}");
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
fn manual_next_records_signals_then_advances() {
    let mut app = app_playing(3, 0);
    let id = current(&app).to_owned();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
    // The skipped track is persisted (SaveSignals) and playback advances.
    assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveSignals)));
    assert_ne!(current(&app), id);
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
        app.bridges.mouse_buttons.borrow().iter().map(|b| b.target).collect()
    };

    // Graphics: a Select (ThemePreset, field 0), a Toggle (BackgroundNone, field 1), and a
    // Text color row (first ThemeColor, field 2).
    let g = render_targets(SettingsTab::Graphics);
    let has = |ts: &[MouseTarget], t: MouseTarget| ts.contains(&t);
    assert!(has(&g, MouseTarget::SettingsChange { row: 0, delta: -1 }), "preset ‹ arrow");
    assert!(has(&g, MouseTarget::SettingsChange { row: 0, delta: 1 }), "preset › arrow");
    assert!(has(&g, MouseTarget::SettingsChange { row: 1, delta: 1 }), "background toggle");
    assert!(has(&g, MouseTarget::SettingsActivate(2)), "color row enters hex editor");
    // Headers are render-only — a click on one falls through to nothing, never a field.

    // Playback leads with the Speed slider (field 0): its ‹ › step arrows are click targets.
    let p = render_targets(SettingsTab::Playback);
    assert!(has(&p, MouseTarget::SettingsChange { row: 0, delta: -1 }), "speed ‹ arrow");
    assert!(has(&p, MouseTarget::SettingsChange { row: 0, delta: 1 }), "speed › arrow");

    // General's Reset buttons (no value) activate on click.
    let general = render_targets(SettingsTab::General);
    let reset_all = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ResetAll)
        .unwrap();
    assert!(has(&general, MouseTarget::SettingsActivate(reset_all)), "reset-all button");
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
            .bridges.mouse_buttons
            .borrow()
            .iter()
            .find(|b| b.target == want)
            .map(|b| b.rect)
            .unwrap_or_else(|| panic!("no rect registered for {want:?}"));
        let buf = terminal.backend().buffer().clone();
        buf.cell((rect.x, rect.y)).map(|c| c.symbol().to_owned()).unwrap_or_default()
    };

    // Speed slider (Playback field 0): the −/+ rects sit on the ‹ › step arrows.
    let dec = MouseTarget::SettingsChange { row: 0, delta: -1 };
    let inc = MouseTarget::SettingsChange { row: 0, delta: 1 };
    assert_eq!(cell_at(SettingsTab::Playback, dec), "‹", "speed decrease lands on ‹");
    assert_eq!(cell_at(SettingsTab::Playback, inc), "›", "speed increase lands on ›");
    // ThemePreset (Graphics field 0): a Select, so the arrows are < >.
    assert_eq!(cell_at(SettingsTab::Graphics, dec), "<", "preset decrease lands on <");
    assert_eq!(cell_at(SettingsTab::Graphics, inc), ">", "preset increase lands on >");
    // BackgroundNone (Graphics field 1): a Toggle, rect over the [ ] / [x] checkbox.
    let toggle = MouseTarget::SettingsChange { row: 1, delta: 1 };
    assert_eq!(cell_at(SettingsTab::Graphics, toggle), "[", "background toggle lands on [");
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
    // The render loop refreshes the art on any change to this mask, so every art-covering
    // popup needs its own bit — switching one straight to another, or stacking a second over
    // a first, must register as an edge.
    let mut app = app_playing(1, 0);
    assert_eq!(app.art_overlay_mask(), 0);
    app.dropdowns.eq_open = true;
    assert_eq!(app.art_overlay_mask(), 0b001);
    // Switch eq -> radio: the mask still changes (0b001 -> 0b010) even though some popup
    // stays open across the switch.
    app.dropdowns.eq_open = false;
    app.dropdowns.radio_open = true;
    assert_eq!(app.art_overlay_mask(), 0b010);
    // The queue window is a distinct bit, and can stack with a dropdown.
    app.queue_popup.open = true;
    assert_eq!(app.art_overlay_mask(), 0b110);
    app.dropdowns.radio_open = false;
    assert_eq!(app.art_overlay_mask(), 0b100);
    app.queue_popup.open = false;
    assert_eq!(app.art_overlay_mask(), 0);
    // The About card covers the art too, so it gets its own bit (and the clean repaint).
    app.about_visible = true;
    assert_eq!(app.art_overlay_mask(), 0b1000);
    app.about_visible = false;
    assert_eq!(app.art_overlay_mask(), 0);
}

#[test]
fn rendering_player_registers_radio_menu_when_autoplay_on() {
    let mut app = app_playing(2, 0);
    app.autoplay_radio = true;
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    assert!(
        app.bridges.mouse_buttons
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

/// The center cell of the hit rect registered for `target` in the last render.
fn button_center(app: &App, target: MouseTarget) -> (u16, u16) {
    app.bridges.mouse_buttons
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

#[test]
fn every_screen_renders_the_nav_bar() {
    for mode in [Mode::Player, Mode::Search, Mode::Library, Mode::Settings, Mode::Ai] {
        let mut app = app_playing(1, 0);
        app.navigate_to(mode);
        render_app(&app);
        let buttons = app.bridges.mouse_buttons.borrow();
        for nav in [Mode::Player, Mode::Search, Mode::Library, Mode::Settings, Mode::Ai] {
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
    assert!(matches!(cmds.as_slice(), [Cmd::Search(q)] if q == "lofi beats"));
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
    render_app(&app); // publishes queue_popup_rect
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
    // Drag down to row 2: anchor stays at 0, so the selection spans 0..=2.
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));
    app.update(Msg::MouseDrag { col, row });
    assert_eq!(app.queue_popup.anchor, 0);
    assert_eq!(app.queue_popup.cursor, 2);
    // The Delete key removes the whole selected range at once.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.len(), 2);
    let ids: Vec<&str> = app.queue.ordered().iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, vec!["id3", "id4"]);
}
