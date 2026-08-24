//! Sleep-timer reducer tests: popup editing, arming/cancel, the fade tick, and the fire
//! path's canonical player intents.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::player::PlayerCmd;
use yututui_core::sleep_timer::SleepTimer;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn player_intent_labels(cmds: &[Cmd]) -> Vec<String> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent.label.to_string()),
            _ => None,
        })
        .collect()
}

fn player_intent_commands(cmds: &[Cmd]) -> Vec<(String, PlayerCmd)> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some((
                intent.label.to_string(),
                intent.commands.first().cloned().expect("one command"),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn popup_opens_with_configured_preset_and_typing_arms() {
    let mut app = App::new(100);
    app.open_sleep_popup();
    let popup = app.sleep_popup.as_ref().expect("popup open");
    assert_eq!(popup.input, "30");
    assert!(!popup.error);

    // Replace the preset with a typed value: backspaces then digits.
    for _ in 0..2 {
        app.on_key_sleep_popup(key(KeyCode::Backspace));
    }
    app.on_key_sleep_popup(key(KeyCode::Char('5')));
    let cmds = app.on_key_sleep_popup(key(KeyCode::Enter));
    assert!(app.sleep_popup.is_none(), "popup closes on commit");
    assert!(cmds.is_empty(), "arming emits no player intents");
    assert!(app.sleep_timer.is_some(), "timer armed");
    let remaining = app
        .sleep_timer
        .and_then(|timer| timer.remaining_secs(Instant::now()))
        .expect("armed timer counts down");
    assert!(
        (295..=300).contains(&remaining),
        "5-minute arm: {remaining}s"
    );
    assert!(
        !app.status.text.is_empty(),
        "arm announces on the status line"
    );
}

#[test]
fn popup_rejects_garbage_and_reopens_with_error() {
    let mut app = App::new(100);
    app.open_sleep_popup();
    for _ in 0..2 {
        app.on_key_sleep_popup(key(KeyCode::Backspace));
    }
    app.on_key_sleep_popup(key(KeyCode::Char('x')));
    // Non-digit characters never enter the buffer, so clear it and type letters via
    // direct state to exercise the parse error path.
    app.sleep_popup.as_mut().expect("popup").input.clear();
    app.sleep_popup
        .as_mut()
        .expect("popup")
        .input
        .push_str("abc");
    let cmds = app.on_key_sleep_popup(key(KeyCode::Enter));
    assert!(cmds.is_empty());
    assert!(app.sleep_timer.is_none(), "no timer from garbage");
    assert!(
        app.sleep_popup.as_ref().is_some_and(|popup| popup.error),
        "popup reopens with the error hint"
    );
}

#[test]
fn popup_off_cancels_an_armed_timer() {
    let mut app = App::new(100);
    app.arm_sleep_timer(30);
    assert!(app.sleep_timer.is_some());
    app.open_sleep_popup();
    // Typing `off` from the untouched preset replaces it and cancels on Enter.
    for c in ['o', 'f', 'f'] {
        app.on_key_sleep_popup(key(KeyCode::Char(c)));
    }
    let cmds = app.on_key_sleep_popup(key(KeyCode::Enter));
    assert!(app.sleep_timer.is_none(), "cancel clears the timer");
    assert!(cmds.is_empty(), "cancel before the fade emits no intents");
    assert!(!app.status.text.is_empty());
}

#[test]
fn typed_digit_replaces_the_untouched_preset() {
    let mut app = App::new(100);
    app.open_sleep_popup();
    app.on_key_sleep_popup(key(KeyCode::Char('1')));
    let cmds = app.on_key_sleep_popup(key(KeyCode::Enter));
    assert!(cmds.is_empty());
    let remaining = app
        .sleep_timer
        .and_then(|timer| timer.remaining_secs(Instant::now()))
        .expect("armed");
    assert!(
        (55..=60).contains(&remaining),
        "one-minute arm, got {remaining}s"
    );
}

#[test]
fn esc_closes_popup_without_arming() {
    let mut app = App::new(100);
    app.open_sleep_popup();
    let cmds = app.on_key_sleep_popup(key(KeyCode::Esc));
    assert!(app.sleep_popup.is_none());
    assert!(app.sleep_timer.is_none());
    assert!(cmds.is_empty());
}

#[test]
fn tick_fires_past_deadline_and_restores_pre_fade_volume() {
    let mut app = App::new(100);
    app.playback.volume = 100;
    app.playback.paused = false;
    // A timer whose deadline already passed with a recorded pre-fade volume.
    app.sleep_timer = Some(SleepTimer {
        deadline: Instant::now() - Duration::from_secs(1),
        fade: Duration::from_secs(30),
        pre_fade_volume: Some(64),
        fading: true,
        last_sent: Some(0),
    });
    let cmds = app.handle_sleep_tick();
    assert!(app.sleep_timer.is_none(), "fire clears the timer");
    let intents = player_intent_commands(&cmds);
    assert_eq!(
        intents.len(),
        2,
        "restore volume + pause, got labels {:?}",
        player_intent_labels(&cmds)
    );
    assert_eq!(intents[0].0, "sleep_restore");
    assert!(matches!(intents[0].1, PlayerCmd::SetVolume(64)));
    assert_eq!(intents[1].0, "sleep_fire");
    assert!(matches!(
        intents[1].1,
        PlayerCmd::SetProperty { ref name, .. } if name == "pause"
    ));
    assert!(
        !app.status.text.is_empty(),
        "fire announces on the status line"
    );
}

#[test]
fn tick_fades_through_the_canonical_set_volume_path() {
    let mut app = App::new(100);
    app.playback.volume = 100;
    // Arm then move the deadline to 10 s from now: the fade window is exactly "now".
    let mut timer = SleepTimer::armed(Instant::now(), 1, 10);
    timer.deadline = Instant::now() + Duration::from_secs(10);
    app.sleep_timer = Some(timer);
    // The fade window began: the first tick emits one volume step through player_intent.
    let cmds = app.handle_sleep_tick();
    let intents = player_intent_commands(&cmds);
    assert_eq!(
        intents.len(),
        1,
        "one fade step per tick, got labels {:?}",
        player_intent_labels(&cmds)
    );
    assert_eq!(intents[0].0, "sleep_fade");
    assert!(matches!(intents[0].1, PlayerCmd::SetVolume(v) if v < 100));
    assert!(
        app.sleep_timer.is_some(),
        "timer stays armed during the fade"
    );
}

#[test]
fn tick_is_inert_without_an_armed_timer() {
    let mut app = App::new(100);
    let cmds = app.handle_sleep_tick();
    assert!(cmds.is_empty());
}

#[test]
fn chapter_tag_and_seekbar_ticks_render_for_chaptered_tracks() {
    let mut app = App::new(100);
    app.playback.duration = Some(600.0);
    app.playback.time_pos = Some(400.0);
    app.playback.time_pos_at = Some(Instant::now());
    app.playback.chapters = vec![
        crate::player::Chapter {
            title: "Chapter One".into(),
            start_secs: 0.0,
        },
        crate::player::Chapter {
            title: "Chapter Two".into(),
            start_secs: 300.0,
        },
    ];
    let buffer = render_app_buffer(&app, 90, 26);
    // The status line names the active chapter.
    assert!(
        buffer_contains(&buffer, "Chapter Two"),
        "status line should name the current chapter"
    );
    // The seekbar row carries at least one interior tick (the frame borders also use │).
    let seekbar_row = (0..buffer.area.height)
        .find(|y| buffer_row(&buffer, *y).contains(" /"))
        .expect("seekbar row with the time label");
    let row = buffer_row(&buffer, seekbar_row);
    let ticks = row.matches('│').count();
    assert!(
        ticks >= 3,
        "expected chapter ticks on the seekbar, got {row}"
    );
}

#[test]
fn sleep_countdown_renders_in_the_status_line() {
    let mut app = App::new(100);
    app.sleep_timer = Some(SleepTimer::armed(Instant::now(), 30, 30));
    let buffer = render_app_buffer(&app, 90, 26);
    assert!(
        buffer_contains(&buffer, "⏾"),
        "status line should show the sleep countdown"
    );
}

#[test]
fn chapter_jump_seeks_to_the_next_boundary_through_the_intent_path() {
    let mut app = App::new(100);
    app.playback.duration = Some(600.0);
    app.playback.time_pos = Some(10.0);
    app.playback.chapters = vec![
        crate::player::Chapter {
            title: "One".into(),
            start_secs: 0.0,
        },
        crate::player::Chapter {
            title: "Two".into(),
            start_secs: 120.0,
        },
        crate::player::Chapter {
            title: "Three".into(),
            start_secs: 300.0,
        },
    ];
    let epoch_before = app.playback.position_epoch;
    let mut cmds = app.jump_chapter(true);
    let intents = player_intent_commands(&cmds);
    assert_eq!(
        intents.len(),
        1,
        "one seek intent, got labels {:?}",
        player_intent_labels(&cmds)
    );
    assert!(matches!(
        intents[0].1,
        PlayerCmd::SeekAbsolute { seconds, .. } if (seconds - 120.0).abs() < f64::EPSILON
    ));
    admit_player_transition(&mut app, &mut cmds);
    assert!(
        app.playback.position_epoch > epoch_before,
        "chapter seek bumps position_epoch through the central path"
    );

    // A track without chapters explains itself instead of seeking.
    let mut plain = App::new(100);
    plain.playback.duration = Some(600.0);
    plain.playback.chapters.clear();
    let cmds = plain.jump_chapter(true);
    assert!(cmds.is_empty());
    assert!(!plain.status.text.is_empty());
}
