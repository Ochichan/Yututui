use super::*;

fn timed_lines(times: &[f64]) -> std::sync::Arc<[LyricLine]> {
    times
        .iter()
        .enumerate()
        .map(|(index, time)| LyricLine {
            time: *time,
            text: format!("lyric {index:02}"),
        })
        .collect::<Vec<_>>()
        .into()
}

fn synced_app(times: &[f64], position: f64, duration: f64) -> App {
    let mut app = app_playing(2, 0);
    app.lyrics.visible = true;
    app.playback.duration = Some(duration);
    app.playback.time_pos = Some(position);
    app.playback.time_pos_at = Some(Instant::now());
    app.playback.paused = false;
    app.playback.speed = 1.0;
    app.update(Msg::LyricsResult {
        video_id: current(&app).to_owned(),
        lines: timed_lines(times),
    });
    app.dirty = false;
    app
}

fn repeat(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Repeat,
        state: KeyEventState::NONE,
    }
}

fn seek_position(cmds: &[Cmd]) -> Option<f64> {
    cmds.iter()
        .flat_map(Cmd::player_commands)
        .find_map(|command| match command {
            PlayerCmd::SeekAbsolute {
                seconds,
                precision: crate::player::SeekPrecision::Exact,
            } => Some(*seconds),
            _ => None,
        })
}

#[test]
fn keyboard_mouse_and_repeat_share_exact_100ms_steps() {
    let mut app = synced_app(&[0.0, 5.0], 2.0, 10.0);
    let epoch = app.playback.position_epoch;

    assert!(app.update(Msg::Key(key(KeyCode::Char('z')))).is_empty());
    assert_eq!(app.lyrics.delay.steps(), -1);
    assert_eq!(app.playback.position_epoch, epoch);

    assert!(
        app.on_mouse_target(MouseTarget::LyricsDelayLater {
            video_id: "id0".into(),
        })
        .is_empty()
    );
    assert_eq!(app.lyrics.delay.steps(), 0);

    assert!(app.update(Msg::Key(repeat(KeyCode::Char('z')))).is_empty());
    assert_eq!(app.lyrics.delay.steps(), -1);

    app.keymap
        .rebind(
            KeyContext::Player,
            Action::LyricsDelayLater,
            crate::keymap::parse_chord("f6").unwrap(),
        )
        .unwrap();
    app.update(Msg::Key(repeat(KeyCode::F(6))));
    assert_eq!(app.lyrics.delay.steps(), 0, "repeat follows a remap");
    app.update(Msg::Key(repeat(KeyCode::Char('Z'))));
    assert_eq!(
        app.lyrics.delay.steps(),
        0,
        "the superseded character repeat is ignored"
    );

    app.overlays.help_visible = true;
    app.update(Msg::Key(repeat(KeyCode::F(6))));
    assert!(app.overlays.help_visible, "repeat cannot dismiss a modal");
    assert_eq!(app.lyrics.delay.steps(), 0);
    app.overlays.help_visible = false;
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    app.update(Msg::Key(repeat(KeyCode::Char('z'))));
    assert!(app.search.input.is_empty());
    assert_eq!(
        app.lyrics.delay.steps(),
        0,
        "repeat cannot leak into text input"
    );
}

#[test]
fn delay_signs_match_highlighting_and_click_seek_math() {
    let mut app = synced_app(&[0.0, 5.0, 9.0], 5.05, 10.0);
    app.playback.paused = true;
    let now = Instant::now();

    app.lyrics.delay = crate::lyrics::LyricDelay::from_steps(1);
    assert!(app.refresh_lyrics_active_at(now));
    assert_eq!(app.lyrics.active_index, Some(0));
    assert_eq!(app.lyrics_line_seek_target("id0", 1), Some(5.1));

    app.lyrics.delay = crate::lyrics::LyricDelay::from_steps(-1);
    assert!(app.refresh_lyrics_active_at(now));
    assert_eq!(app.lyrics.active_index, Some(1));
    assert_eq!(app.lyrics_line_seek_target("id0", 1), Some(4.9));
    assert_eq!(app.lyrics_line_seek_target("id0", 0), Some(0.0));

    app.lyrics.delay = crate::lyrics::LyricDelay::from_steps(20);
    assert_eq!(app.lyrics_line_seek_target("id0", 2), Some(10.0));
}

#[test]
fn admitted_track_identity_alone_resets_the_session_delay() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    app.lyrics.delay = crate::lyrics::LyricDelay::from_steps(4);
    let owner = app.lyrics.delay_video_id.clone();

    let mut same = app.load_song(app.queue.current().cloned());
    assert_eq!(app.lyrics.delay.steps(), 4, "planning is non-mutating");
    admit_player_transition(&mut app, &mut same);
    assert_eq!(app.lyrics.delay.steps(), 4);
    assert_eq!(app.lyrics.delay_video_id, owner);

    let mut next = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert_eq!(
        app.lyrics.delay.steps(),
        4,
        "admission has not happened yet"
    );
    admit_player_transition(&mut app, &mut next);
    assert_eq!(current(&app), "id1");
    assert_eq!(app.lyrics.delay.steps(), 0);
    assert_eq!(app.lyrics.delay_video_id.as_deref(), Some("id1"));

    let mut rejected = synced_app(&[0.0, 5.0], 1.0, 10.0);
    rejected.lyrics.delay = crate::lyrics::LyricDelay::from_steps(3);
    let next = rejected.update(Msg::Key(key(KeyCode::Char('.'))));
    reject_player_transition(
        &mut rejected,
        next,
        crate::util::delivery::DeliveryError::Busy,
    );
    assert_eq!(current(&rejected), "id0");
    assert_eq!(rejected.lyrics.delay.steps(), 3);
}

#[test]
fn hidden_empty_stale_live_and_unloaded_lyrics_fail_closed() {
    let target = || MouseTarget::LyricsLine {
        video_id: "id0".into(),
        line_index: 1,
    };

    let mut hidden = synced_app(&[0.0, 5.0], 1.0, 10.0);
    hidden.lyrics.visible = false;
    hidden.update(Msg::Key(key(KeyCode::Char('z'))));
    assert_eq!(hidden.lyrics.delay.steps(), 0);
    assert!(hidden.on_mouse_target(target()).is_empty());

    let mut empty = synced_app(&[0.0, 5.0], 1.0, 10.0);
    empty.lyrics.track.as_mut().unwrap().lines = std::sync::Arc::from([]);
    empty.update(Msg::Key(key(KeyCode::Char('Z'))));
    assert_eq!(empty.lyrics.delay.steps(), 0);
    assert!(empty.on_mouse_target(target()).is_empty());

    let mut stale = synced_app(&[0.0, 5.0], 1.0, 10.0);
    assert!(
        stale
            .on_mouse_target(MouseTarget::LyricsLine {
                video_id: "old-id".into(),
                line_index: 1,
            })
            .is_empty()
    );
    assert!(
        stale
            .on_mouse_target(MouseTarget::LyricsDelayEarlier {
                video_id: "old-id".into(),
            })
            .is_empty()
    );
    assert_eq!(stale.lyrics.delay.steps(), 0);
    stale.lyrics.loading = true;
    stale.update(Msg::LyricsResult {
        video_id: "old-id".to_owned(),
        lines: timed_lines(&[0.0]),
    });
    assert!(stale.lyrics.loading, "a stale reply cannot clear loading");

    let mut unloaded = synced_app(&[0.0, 5.0], 1.0, 10.0);
    unloaded.prefetch.loaded_video_id = None;
    unloaded.update(Msg::Key(key(KeyCode::Char('z'))));
    assert_eq!(unloaded.lyrics.delay.steps(), 0);
    assert!(unloaded.on_mouse_target(target()).is_empty());

    let mut live = radio_playing("live");
    live.lyrics.visible = true;
    live.playback.duration = Some(10.0);
    live.lyrics.track = Some(TrackLyrics {
        video_id: current(&live).into(),
        lines: timed_lines(&[0.0, 5.0]),
    });
    live.update(Msg::Key(key(KeyCode::Char('z'))));
    assert_eq!(live.lyrics.delay.steps(), 0);
    assert!(
        live.on_mouse_target(MouseTarget::LyricsLine {
            video_id: current(&live).into(),
            line_index: 1,
        })
        .is_empty()
    );
}

#[test]
fn lyric_click_mutates_position_only_after_admission_and_bumps_epoch_once() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    app.playback.paused = true;
    app.lyrics.delay = crate::lyrics::LyricDelay::from_steps(1);
    let before_position = app.playback.time_pos;
    let before_epoch = app.playback.position_epoch;
    let cmds = app.on_mouse_target(MouseTarget::LyricsLine {
        video_id: "id0".into(),
        line_index: 1,
    });
    assert_eq!(seek_position(&cmds), Some(5.1));
    assert_eq!(app.playback.time_pos, before_position);
    assert_eq!(app.playback.position_epoch, before_epoch);

    let mut admitted = cmds;
    admit_player_transition(&mut app, &mut admitted);
    assert_eq!(app.playback.time_pos, Some(5.1));
    assert_eq!(
        app.lyrics.active_index,
        Some(1),
        "an admitted paused seek repairs the stored highlight"
    );
    assert!(app.dirty, "the repaired paused highlight requests a redraw");
    assert_eq!(app.playback.position_epoch, before_epoch + 1);

    let mut rejected = synced_app(&[0.0, 5.0], 1.0, 10.0);
    let epoch = rejected.playback.position_epoch;
    let position = rejected.playback.time_pos;
    let cmds = rejected.on_mouse_target(MouseTarget::LyricsLine {
        video_id: "id0".into(),
        line_index: 1,
    });
    reject_player_transition(
        &mut rejected,
        cmds,
        crate::util::delivery::DeliveryError::Busy,
    );
    assert_eq!(rejected.playback.time_pos, position);
    assert_eq!(rejected.playback.position_epoch, epoch);
}

#[test]
fn paused_surface_return_reconciles_the_highlight_without_a_clock_tick() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    app.playback.paused = true;
    assert_eq!(app.lyrics.active_index, Some(0));

    app.mode = Mode::Search;
    app.update(PlayerMsg::TimePos(6.0));
    assert_eq!(
        app.lyrics.active_index,
        Some(0),
        "off-screen state stays parked"
    );
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.lyrics.active_index, Some(1));

    app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
    app.update(PlayerMsg::TimePos(1.0));
    assert_eq!(app.lyrics.active_index, Some(1), "Mini stays parked");
    app.update(Msg::TerminalResize {
        width: 80,
        height: 24,
    });
    assert_eq!(
        app.lyrics.active_index,
        Some(0),
        "leaving Mini reconciles before the full frame"
    );
}

#[test]
fn initial_osd_waits_until_lyrics_can_really_be_shown() {
    let payload = || timed_lines(&[0.0, 5.0]);

    let mut hidden = app_playing(1, 0);
    hidden.update(Msg::LyricsResult {
        video_id: current(&hidden).to_owned(),
        lines: payload(),
    });
    assert!(hidden.lyrics.initial_osd_pending);
    assert!(hidden.lyrics.delay_osd_until.is_none());
    hidden.update(Msg::Key(key(KeyCode::Char('L'))));
    assert!(!hidden.lyrics.initial_osd_pending);
    assert!(hidden.lyrics.delay_osd_until.is_some());

    let mut offscreen = app_playing(1, 0);
    offscreen.lyrics.visible = true;
    offscreen.mode = Mode::Search;
    offscreen.update(Msg::LyricsResult {
        video_id: current(&offscreen).to_owned(),
        lines: payload(),
    });
    assert!(offscreen.lyrics.initial_osd_pending);
    assert!(offscreen.lyrics.delay_osd_until.is_none());
    assert!(!offscreen.expire_lyrics_delay_osd(Instant::now() + Duration::from_secs(30)));
    assert!(
        offscreen.lyrics.initial_osd_pending,
        "wall time cannot consume a pending first exposure"
    );
    offscreen.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert!(!offscreen.lyrics.initial_osd_pending);
    assert!(offscreen.lyrics.delay_osd_until.is_some());

    let mut mini = app_playing(1, 0);
    mini.lyrics.visible = true;
    mini.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
    mini.update(Msg::LyricsResult {
        video_id: current(&mini).to_owned(),
        lines: payload(),
    });
    assert!(mini.lyrics.initial_osd_pending);
    assert!(mini.lyrics.delay_osd_until.is_none());
    mini.update(Msg::TerminalResize {
        width: 80,
        height: 24,
    });
    assert!(!mini.lyrics.initial_osd_pending);
    assert!(mini.lyrics.delay_osd_until.is_some());

    mini.update(Msg::LyricsResult {
        video_id: current(&mini).to_owned(),
        lines: timed_lines(&[]),
    });
    assert!(!mini.lyrics.initial_osd_pending);
    assert!(mini.lyrics.delay_osd_until.is_none());
}

#[test]
fn pre_render_tier_sync_repairs_zoom_only_mini_exit_on_the_first_full_frame() {
    let mut app = app_playing(1, 0);
    app.lyrics.visible = true;
    app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
    app.update(Msg::LyricsResult {
        video_id: current(&app).to_owned(),
        lines: timed_lines(&[0.0, 5.0]),
    });
    app.playback.paused = true;
    app.playback.time_pos = Some(6.0);
    app.lyrics.active_index = Some(0);

    app.prepare_ui_tier_for_render(crate::ui::layout::UiTier::Full);

    assert_eq!(app.lyrics.active_index, Some(1));
    assert!(!app.lyrics.initial_osd_pending);
    assert!(app.lyrics.delay_osd_until.is_some());
}

#[test]
fn osd_arms_refreshes_expires_reopens_and_blocks_click_through() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    assert!(
        app.lyrics.delay_osd_until.is_some(),
        "first payload opens OSD"
    );
    assert!(buffer_contains(
        &render_app_buffer(&app, 80, 24),
        "[ − 0.0s + ]"
    ));

    let start = Instant::now();
    assert!(app.arm_lyrics_delay_osd(start));
    assert_eq!(
        app.lyrics.delay_osd_until,
        start.checked_add(Duration::from_secs(3))
    );
    assert!(app.adjust_lyrics_delay(LyricsDelayDirection::Later, start + Duration::from_secs(1)));
    assert_eq!(
        app.lyrics.delay_osd_until,
        start.checked_add(Duration::from_secs(4))
    );
    assert!(!app.expire_lyrics_delay_osd(start + Duration::from_secs(3)));
    assert!(app.expire_lyrics_delay_osd(start + Duration::from_secs(4)));

    app.dirty = false;
    assert!(
        app.on_mouse_target(MouseTarget::LyricsDelayHandle {
            video_id: "old-id".into(),
        })
        .is_empty()
    );
    assert!(app.lyrics.delay_osd_until.is_none());
    assert!(
        app.on_mouse_target(MouseTarget::LyricsDelayHandle {
            video_id: "id0".into(),
        })
        .is_empty()
    );
    assert!(app.lyrics.delay_osd_until.is_some());
    assert!(app.dirty);

    let buffer = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buffer, "[ − +0.1s + ]"));
    let blocker = app
        .hits
        .regions()
        .iter()
        .find(|region| region.target == MouseTarget::LyricsDelayBlock)
        .cloned()
        .expect("expanded OSD blocker");
    assert_eq!(
        app.hits.target_at(blocker.rect.x + 4, blocker.rect.y),
        Some(MouseTarget::LyricsDelayBlock)
    );
    assert_eq!(
        app.hits.target_at(blocker.rect.x + 2, blocker.rect.y),
        Some(MouseTarget::LyricsDelayEarlier {
            video_id: "id0".into()
        })
    );
    let delay = app.lyrics.delay;
    app.dirty = false;
    assert!(
        app.update(Msg::MouseClick {
            col: blocker.rect.x + 4,
            row: blocker.rect.y,
            multi: false,
        })
        .is_empty()
    );
    assert_eq!(app.lyrics.delay, delay);
    assert!(!app.dirty, "the value is inert and consumes the click");

    app.lyrics.delay_osd_until = None;
    render_app(&app);
    let handle = app
        .hits
        .regions()
        .iter()
        .find(|region| matches!(region.target, MouseTarget::LyricsDelayHandle { .. }))
        .cloned()
        .expect("collapsed handle");
    assert!(matches!(
        app.hits.target_at(handle.rect.x, handle.rect.y),
        Some(MouseTarget::LyricsDelayHandle { .. })
    ));
}

#[test]
fn rendered_lyric_targets_share_the_track_video_id_allocation() {
    let app = synced_app(&[0.0, 5.0, 9.0], 5.05, 10.0);
    let owner = app
        .lyrics
        .track
        .as_ref()
        .expect("loaded lyrics")
        .video_id
        .clone();
    let _buffer = render_app_buffer(&app, 80, 24);
    let mut shared_targets = 0usize;
    let regions = app.hits.regions();
    for region in regions.iter() {
        let target_id = match &region.target {
            MouseTarget::LyricsLine { video_id, .. }
            | MouseTarget::LyricsDelayHandle { video_id }
            | MouseTarget::LyricsDelayEarlier { video_id }
            | MouseTarget::LyricsDelayLater { video_id } => Some(video_id),
            _ => None,
        };
        if let Some(video_id) = target_id {
            assert!(std::sync::Arc::ptr_eq(&owner, video_id));
            shared_targets += 1;
        }
    }
    assert!(shared_targets > 1, "expected lyric rows and an OSD target");
}

#[test]
fn lyric_clock_redraws_only_at_boundaries_and_parks_outside_active_player() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    let anchor = Instant::now();
    app.playback.time_pos_at = Some(anchor);
    app.lyrics.active_index = Some(0);
    assert!(app.lyrics_clock_active());

    app.dirty = false;
    app.update(Msg::LyricsTick);
    assert!(!app.dirty, "same active row does not redraw");
    app.playback.time_pos = Some(4.95);
    app.playback.time_pos_at = Some(anchor);
    assert!(app.lyrics_tick_at(anchor + Duration::from_millis(100)));
    assert_eq!(app.lyrics.active_index, Some(1));

    app.lyrics.visible = false;
    assert!(!app.lyrics_clock_active());
    app.lyrics.visible = true;
    app.playback.paused = true;
    assert!(!app.lyrics_clock_active());
    app.playback.paused = false;
    app.focused = false;
    assert!(!app.lyrics_clock_active());
    app.focused = true;
    app.mode = Mode::Search;
    assert!(!app.lyrics_clock_active());
    app.mode = Mode::Player;
    app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
    assert!(!app.lyrics_clock_active());
}

#[test]
fn animation_tick_reconciles_the_lyric_row_before_arming_its_flash() {
    let mut app = synced_app(&[0.0, 5.0], 1.0, 10.0);
    app.config.animations.master = true;
    app.config.animations.lyrics = true;
    app.playback.time_pos = Some(5.1);
    app.playback.time_pos_at = Some(Instant::now());
    app.lyrics.active_index = Some(0);
    app.fx.last_lyric_index = Some(0);
    app.fx.lyric = None;
    let expected_start = app.anim_frame().wrapping_add(1);

    app.dirty = false;
    app.update(Msg::AnimTick);

    assert_eq!(app.lyrics.active_index, Some(1));
    assert_eq!(app.fx.last_lyric_index, Some(1));
    assert_eq!(app.fx.lyric, Some(expected_start));
    assert!(app.dirty);
}

#[test]
fn render_targets_stay_visible_osd_wins_and_art_never_overlaps() {
    let times = (0..24).map(f64::from).collect::<Vec<_>>();
    for position in [
        crate::config::PlayerBarPosition::Top,
        crate::config::PlayerBarPosition::Bottom,
    ] {
        for with_art in [false, true] {
            for (width, height) in [(80, 24), (32, 14)] {
                let mut app = synced_app(&times, 12.2, 30.0);
                app.config.player_bar_position = Some(position);
                if with_art {
                    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);
                }
                let overlay_mask = app.art.overlay_mask;
                let buffer = render_app_buffer(&app, width, height);
                let screen = Rect::new(0, 0, width, height);
                let regions = app.hits.regions().to_vec();
                let lyric_rows = regions
                    .iter()
                    .filter(|region| matches!(region.target, MouseTarget::LyricsLine { .. }))
                    .collect::<Vec<_>>();
                assert!(
                    !lyric_rows.is_empty(),
                    "{position:?} art={with_art} {width}x{height}"
                );
                assert!(
                    lyric_rows.len() < times.len(),
                    "only visible rows are registered"
                );
                let blocker = regions
                    .iter()
                    .find(|region| region.target == MouseTarget::LyricsDelayBlock)
                    .expect("expanded OSD remains inside the lyric area");
                assert_eq!(
                    app.hits.target_at(blocker.rect.x + 2, blocker.rect.y),
                    Some(MouseTarget::LyricsDelayEarlier {
                        video_id: "id0".into()
                    }),
                    "OSD button wins over the lyric row beneath it"
                );
                for region in regions.iter().filter(|region| {
                    matches!(
                        region.target,
                        MouseTarget::LyricsLine { .. }
                            | MouseTarget::LyricsDelayHandle { .. }
                            | MouseTarget::LyricsDelayEarlier { .. }
                            | MouseTarget::LyricsDelayLater { .. }
                            | MouseTarget::LyricsDelayBlock
                    )
                }) {
                    assert_eq!(region.rect.intersection(screen), region.rect);
                    if let Some(art) = app.art.rect.get() {
                        assert!(
                            region.rect.intersection(art).is_empty(),
                            "lyric control {:?} overlaps art {art:?}",
                            region.target
                        );
                    }
                    if !matches!(region.target, MouseTarget::LyricsLine { .. }) {
                        for y in region.rect.top()..region.rect.bottom() {
                            for x in region.rect.left()..region.rect.right() {
                                assert_eq!(
                                    buffer[(x, y)].bg,
                                    app.theme.color(crate::theme::ThemeRole::Background),
                                    "OSD uses the theme background without clearing the panel"
                                );
                            }
                        }
                    }
                }
                assert_eq!(
                    app.art.overlay_mask, overlay_mask,
                    "OSD claimed an overlay bit"
                );
            }
        }
    }
}
