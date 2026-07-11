use super::*;

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
            .collect::<Vec<_>>()
            .into(),
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
                app.update(Msg::AnimTick(1));
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
fn idle_event_feedback_does_not_keep_the_player_clock_awake() {
    let mut app = app_playing(1, 0);
    let a = &mut app.config.animations;
    a.master = true;
    a.like_burst = true;
    a.track_intro = true;
    a.seek_flash = true;
    a.volume_flash = true;
    a.toast = true;
    assert!(a.active(), "the configured flags are enabled");
    assert!(!app.fx_active(), "no feedback window is armed");
    assert!(
        !app.animation_active(),
        "idle feedback must not redraw throughout playback"
    );

    app.config.animations.controls = true;
    assert!(
        app.animation_active(),
        "a continuous player effect wakes it"
    );
}

#[test]
fn visible_player_selections_wake_the_ambient_clock() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.selection = true;
    assert!(!app.animation_active(), "no selection surface is visible");

    app.queue_popup.open = true;
    assert!(app.animation_active(), "queue selection should breathe");
    assert_eq!(app.animation_draw_fps(), 12);
    app.queue_popup.open = false;

    app.dropdowns.eq_open = true;
    assert!(app.animation_active(), "EQ selection should breathe");
    app.dropdowns.eq_open = false;
    app.dropdowns.streaming_open = true;
    assert!(
        app.animation_active(),
        "streaming-mode selection should breathe"
    );

    app.playback.paused = true;
    assert!(
        !app.animation_active(),
        "player selections retain the pause gate"
    );
}

#[test]
fn running_download_wakes_the_player_activity_clock() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.activity = true;
    assert!(!app.animation_active(), "no activity is visible");

    let id = app.queue.current().unwrap().video_id.clone();
    app.downloads
        .active
        .insert(id.clone(), DownloadState::Running(42));
    assert!(app.animation_active(), "download spinner should advance");
    assert_eq!(app.animation_draw_fps(), 12);

    app.downloads.active.insert(id, DownloadState::Done);
    assert!(!app.animation_active(), "completed downloads are static");
}

#[test]
fn turning_the_master_off_cancels_armed_feedback() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.volume_flash = true;
    app.update(Msg::Resize);
    app.playback.volume -= 5;
    app.update(Msg::Resize);
    assert!(app.fx_active());

    app.toggle_animations();
    assert!(!app.animations().master);
    assert!(!app.fx_active());
    assert!(!app.animation_active());

    app.toggle_animations();
    assert!(app.animations().master);
    assert!(!app.fx_active(), "cancelled feedback must not resume");
    assert!(
        !app.animation_active(),
        "idle feedback flag keeps the clock asleep"
    );
}

#[test]
fn volume_edge_flash_blinks_color_without_moving_cells() {
    let mut app = app_playing(1, 0);
    app.config.animations.master = true;
    app.config.animations.volume_flash = true;
    let snapshot = |app: &App| {
        let buf = render_app_buffer(app, 80, 20);
        let rect = app
            .hits
            .regions()
            .iter()
            .find(|region| region.target == MouseTarget::VolumeArea)
            .expect("volume hit area")
            .rect;
        let cells = (rect.x..rect.right())
            .map(|x| {
                let cell = &buf[(x, rect.y)];
                (cell.symbol().to_owned(), cell.fg)
            })
            .collect::<Vec<_>>();
        (rect, cells)
    };

    let channel_delta = |a, b| match (a, b) {
        (ratatui::style::Color::Rgb(ar, ag, ab), ratatui::style::Color::Rgb(br, bg, bb)) => {
            ar.abs_diff(br).max(ag.abs_diff(bg)).max(ab.abs_diff(bb))
        }
        pair => panic!("expected RGB flash colors, got {pair:?}"),
    };
    let mut edge_rect = None;
    for volume in [0, 100] {
        app.playback.volume = volume;
        app.fx.volume = Some(app.anim_frame());
        let start = snapshot(&app);
        for _ in 0..app.anim_ms_frames(crate::ui::anim::fx_window::VOLUME_MS) / 2 {
            app.update(Msg::AnimTick(1));
        }
        let peak = snapshot(&app);
        assert_eq!(start.0, peak.0, "volume overlay moved at {volume}%");
        assert_eq!(
            start.1.iter().map(|(symbol, _)| symbol).collect::<Vec<_>>(),
            peak.1.iter().map(|(symbol, _)| symbol).collect::<Vec<_>>()
        );
        assert_ne!(start.1[0].1, peak.1[0].1, "edge blink did not change color");
        assert!(
            channel_delta(start.1[0].1, peak.1[0].1) <= 48,
            "edge blink is too strong"
        );
        if let Some(rect) = edge_rect {
            assert_eq!(start.0, rect, "endpoint gauge widths differ");
        } else {
            edge_rect = Some(start.0);
        }
    }

    app.playback.volume = 50;
    app.fx.volume = Some(app.anim_frame());
    let start = snapshot(&app);
    for _ in 0..app.anim_ms_frames(crate::ui::anim::fx_window::VOLUME_MS) / 2 {
        app.update(Msg::AnimTick(1));
    }
    assert_eq!(
        start,
        snapshot(&app),
        "non-edge volume unexpectedly blinked"
    );
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
fn canvas_variable_wakes_preserve_30_to_20_legacy_due_times_and_frames() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.rain = true;
    app.config.animations.fps = 30;

    assert_eq!(app.animation_tick_fps(), 30);
    assert_eq!(app.animation_draw_fps(), 20);

    let mut due_ms = 0;
    let expected = [(66, 2), (99, 3), (165, 5), (198, 6), (264, 8), (297, 9)];
    for (expected_due_ms, expected_frame) in expected {
        let logical_ticks = app.animation_ticks_until_next_draw();
        due_ms += logical_ticks * 33;
        app.dirty = false;
        app.update(Msg::AnimTick(logical_ticks));
        assert_eq!(due_ms, expected_due_ms);
        assert_eq!(app.anim.anim_frame, expected_frame);
        assert!(app.dirty, "every timer wake must be an actual draw");
    }
}

#[test]
fn mascot_and_marquee_wakes_keep_their_legacy_integer_due_times() {
    let mut app = app_playing(1, 0);
    app.mode = Mode::Ai;
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.fps = 30;

    assert!(app.animation_active());
    assert_eq!(app.animation_tick_fps(), 30);
    assert_eq!(app.animation_draw_fps(), 3);

    let mut due_ms = 0;
    for (expected_due_ms, expected_frame) in [(330, 10), (660, 20), (990, 30)] {
        let logical_ticks = app.animation_ticks_until_next_draw();
        due_ms += logical_ticks * 33;
        app.dirty = false;
        app.update(Msg::AnimTick(logical_ticks));
        assert_eq!(due_ms, expected_due_ms);
        assert_eq!(app.anim.anim_frame, expected_frame);
        assert!(app.dirty);
    }

    let mut marquee = App::new(100);
    marquee.config.animations.fps = 30;
    marquee.bridges.marquee_ran.set(true);
    assert_eq!(marquee.animation_draw_fps(), 5);
    let mut due_ms = 0;
    for (expected_due_ms, expected_frame) in [(198, 6), (396, 12), (594, 18)] {
        let logical_ticks = marquee.animation_ticks_until_next_draw();
        due_ms += logical_ticks * 33;
        marquee.dirty = false;
        marquee.update(Msg::AnimTick(logical_ticks));
        assert_eq!(due_ms, expected_due_ms);
        assert_eq!(marquee.anim_frame(), expected_frame);
        assert!(marquee.dirty);
    }
}

#[test]
fn ambient_draw_sequences_stay_stable_at_5_30_and_60_configured_fps() {
    for (fps, expected_frames) in [
        (5, vec![1, 2, 3, 4, 5]),
        (30, vec![3, 5, 8, 10, 13, 15, 18, 20, 23, 25, 28, 30]),
        (60, vec![5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60]),
    ] {
        let mut app = App::new(100);
        app.mode = Mode::Search;
        app.config.animations.master = true;
        app.config.animations.caret = true;
        app.config.animations.fps = fps;
        for expected_frame in expected_frames {
            let logical_ticks = app.animation_ticks_until_next_draw();
            app.dirty = false;
            app.update(Msg::AnimTick(logical_ticks));
            assert_eq!(app.anim.anim_frame, expected_frame, "configured fps {fps}");
            assert!(app.dirty);
        }
    }
}

#[test]
fn delayed_wake_catches_up_to_latest_visible_phase_without_backlog_draws() {
    let mut between_draws = app_playing(1, 0);
    between_draws.playback.paused = false;
    between_draws.config.animations.master = true;
    between_draws.config.animations.rain = true;
    between_draws.config.animations.fps = 30;

    // Four logical deadlines have elapsed at 150 ms (33/66/99/132), but frame 4 was not a
    // legacy draw. One wake must land on frame 3, the newest actually-visible phase.
    let logical_ticks = between_draws.animation_ticks_through_latest_draw(150 / 33);
    assert_eq!(logical_ticks, 3);
    between_draws.dirty = false;
    between_draws.update(Msg::AnimTick(logical_ticks));
    assert_eq!(between_draws.anim_frame(), 3);
    assert!(between_draws.dirty);
    assert_eq!(between_draws.animation_ticks_until_next_draw(), 2);
    assert_eq!(3 * 33 + 2 * 33, 165);

    // A 200 ms stall crosses four draw deadlines. Catch up all four in the same reducer turn and
    // render only the newest frame, then continue on the legacy grid at 264 ms.
    let mut many_draws = app_playing(1, 0);
    many_draws.playback.paused = false;
    many_draws.config.animations.master = true;
    many_draws.config.animations.rain = true;
    many_draws.config.animations.fps = 30;
    let logical_ticks = many_draws.animation_ticks_through_latest_draw(200 / 33);
    assert_eq!(logical_ticks, 6);
    many_draws.dirty = false;
    many_draws.update(Msg::AnimTick(logical_ticks));
    assert_eq!(many_draws.anim_frame(), 6);
    assert!(many_draws.dirty);
    assert_eq!(many_draws.animation_ticks_until_next_draw(), 2);
    assert_eq!(6 * 33 + 2 * 33, 264);
}

#[test]
fn variable_wakes_match_legacy_visible_buffers_at_fixed_wall_clock_checkpoints() {
    for checkpoint_ms in [
        0_u64, 32, 33, 65, 66, 98, 99, 131, 132, 150, 164, 165, 197, 198, 231, 990, 1024,
    ] {
        let mut legacy = app_playing(1, 0);
        legacy.playback.paused = false;
        legacy.config.animations.master = true;
        legacy.config.animations.rain = true;
        legacy.config.animations.fps = 30;
        let mut legacy_visible = render_app_buffer(&legacy, 80, 24);
        legacy.dirty = false;
        for _ in 0..checkpoint_ms / 33 {
            legacy.update(Msg::AnimTick(1));
            if legacy.dirty {
                legacy_visible = render_app_buffer(&legacy, 80, 24);
                legacy.dirty = false;
            }
        }

        let mut variable = app_playing(1, 0);
        variable.playback.paused = false;
        variable.config.animations.master = true;
        variable.config.animations.rain = true;
        variable.config.animations.fps = 30;
        let mut variable_visible = render_app_buffer(&variable, 80, 24);
        variable.dirty = false;
        let logical_ticks = variable.animation_ticks_through_latest_draw(checkpoint_ms / 33);
        if logical_ticks > 0 {
            variable.update(Msg::AnimTick(logical_ticks));
            assert!(variable.dirty);
            variable_visible = render_app_buffer(&variable, 80, 24);
        }

        assert_eq!(
            variable_visible, legacy_visible,
            "visible buffer diverged at {checkpoint_ms} ms"
        );
    }
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
