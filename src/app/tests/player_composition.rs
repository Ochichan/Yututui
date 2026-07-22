use super::*;

use crate::config::PlayerBarPosition;
use crate::ui::layout::UiTier;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui_image::picker::ProtocolType;
use unicode_width::UnicodeWidthStr;

const LYRIC_MARKER: &str = "SIDE LYRICS";

fn bottom_player_with_art_and_lyrics() -> App {
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    make_test_art_active(&mut app, ProtocolType::Halfblocks);
    app.playback.time_pos = Some(0.0);
    app.playback.time_pos_at = None;
    app.lyrics.visible = true;
    app.lyrics.track = Some(TrackLyrics {
        video_id: app
            .queue
            .current()
            .expect("current track")
            .video_id
            .clone()
            .into(),
        lines: vec![crate::lyrics::LyricLine {
            time: 0.0,
            text: LYRIC_MARKER.to_owned(),
        }]
        .into(),
    });
    app
}

fn text_position(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    (0..buffer.area.height).find_map(|y| {
        let row = buffer_row(buffer, y);
        row.find(needle).map(|byte| {
            let x = UnicodeWidthStr::width(&row[..byte]) as u16;
            (x, y)
        })
    })
}

fn disjoint(a: Rect, b: Rect) -> bool {
    a.intersection(b).is_empty()
}

#[test]
fn bottom_final_art_rects_follow_requested_frame_geometry_and_yield_tiny_space() {
    let _guard = crate::i18n::lock_for_test();
    let app = bottom_player_with_art_and_lyrics();
    let cases = [
        ((160, 50), Some(Rect::new(38, 12, 42, 21))),
        ((100, 30), Some(Rect::new(8, 2, 42, 21))),
        ((80, 24), Some(Rect::new(4, 2, 30, 15))),
        ((60, 18), Some(Rect::new(1, 2, 18, 9))),
        ((32, 14), None),
    ];

    for ((width, height), expected_art) in cases {
        let buffer = render_app_buffer(&app, width, height);
        assert_eq!(
            app.art.rect.get(),
            expected_art,
            "unexpected final art rect at {width}x{height}"
        );
        assert!(
            buffer_contains(&buffer, LYRIC_MARKER),
            "lyrics should retain the filler at {width}x{height}"
        );

        if let Some(art) = expected_art {
            let (lyrics_x, lyrics_y) =
                text_position(&buffer, LYRIC_MARKER).expect("unique lyric marker rendered");
            assert!(
                lyrics_x >= art.right().saturating_add(2),
                "{width}x{height}: lyrics at x={lyrics_x} must sit beyond art {art:?} and its two-cell gap"
            );
            assert!(
                lyrics_y >= 2 && lyrics_y < height.saturating_sub(7),
                "{width}x{height}: lyrics row {lyrics_y} escaped the filler"
            );
        }
    }
}

#[test]
fn top_and_bottom_art_only_match_the_large_frame_preferred_size() {
    let _guard = crate::i18n::lock_for_test();
    for (position, expected) in [
        (PlayerBarPosition::Top, Rect::new(59, 10, 42, 21)),
        (PlayerBarPosition::Bottom, Rect::new(59, 12, 42, 21)),
    ] {
        let mut app = app_playing(2, 0);
        app.config.player_bar_position = Some(position);
        make_test_art_active(&mut app, ProtocolType::Halfblocks);

        let _ = render_app_buffer(&app, 160, 50);
        assert_eq!(app.art.rect.get(), Some(expected));
    }
}

#[test]
fn bottom_art_never_overlaps_controls_or_footer() {
    let _guard = crate::i18n::lock_for_test();
    let app = bottom_player_with_art_and_lyrics();

    for (width, height) in [(160, 50), (100, 30), (80, 24), (60, 18)] {
        let _ = render_app_buffer(&app, width, height);
        let art = app.art.rect.get().expect("art fits this full-tier frame");
        for target in [
            MouseTarget::Player(Action::TogglePause),
            MouseTarget::VolumeArea,
            MouseTarget::Global(Action::ToggleHelp),
            MouseTarget::MouseHelp,
        ] {
            let foreground = app
                .hits
                .rect_of_target(target.clone())
                .unwrap_or_else(|| panic!("{target:?} registered at {width}x{height}"));
            assert!(
                disjoint(art, foreground),
                "{width}x{height}: art {art:?} overlaps {target:?} at {foreground:?}"
            );
        }
        let seekbar = app.hits.seekbar_rect().expect("seekbar registered");
        assert!(
            disjoint(art, seekbar),
            "{width}x{height}: art {art:?} overlaps seekbar {seekbar:?}"
        );
    }
}

#[test]
fn bottom_art_suppresses_only_the_rendered_donut() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.donut = true;
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    make_test_art_active(&mut app, ProtocolType::Halfblocks);

    let _ = render_app_buffer(&app, 100, 30);
    assert!(app.config.animations.donut, "raw config must be retained");
    assert!(
        app.art.rect.get().is_some(),
        "Bottom art is actually visible"
    );
    assert!(
        !app.bridges.canvas_active.get() && !app.bridges.canvas_heavy_active.get(),
        "the only configured canvas effect is contextually suppressed"
    );

    app.config.player_bar_position = Some(PlayerBarPosition::Top);
    let _ = render_app_buffer(&app, 100, 30);
    assert!(
        app.config.animations.donut,
        "layout changes do not edit config"
    );
    assert!(
        app.bridges.canvas_active.get() && app.bridges.canvas_heavy_active.get(),
        "Top restores the configured donut"
    );

    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.config.album_art = Some(false);
    let _ = render_app_buffer(&app, 100, 30);
    assert!(app.art.rect.get().is_none(), "art-off publishes no mask");
    assert!(
        app.bridges.canvas_active.get() && app.bridges.canvas_heavy_active.get(),
        "Bottom restores donut as soon as no final art rect exists"
    );

    app.config.album_art = Some(true);
    app.art.loading = true;
    app.art.protocol.borrow_mut().take();
    let _ = render_app_buffer(&app, 100, 30);
    assert!(
        app.art.rect.get().is_none(),
        "loading art publishes no mask"
    );
    assert!(
        app.bridges.canvas_active.get(),
        "a loading image must not contextually suppress donut"
    );

    app.art
        .picker
        .as_mut()
        .expect("test picker")
        .set_protocol_type(ProtocolType::Kitty);
    let video_id = app.queue.current().expect("current track").video_id.clone();
    app.set_artwork(video_id, Some(image::DynamicImage::new_rgba8(32, 32)));
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    app.zoom.set(125);
    let _ = render_app_buffer(&app, 100, 30);
    assert!(
        app.art.rect.get().is_some(),
        "Kitty art participates in the supported OSC 66 zoom grid"
    );
    assert!(
        !app.bridges.canvas_active.get() && !app.bridges.canvas_heavy_active.get(),
        "visible zoomed Kitty art keeps the bottom donut contextually suppressed"
    );
}

#[test]
fn visible_canvas_bridge_caps_heavy_redraws_even_with_lyrics() {
    let mut app = bottom_player_with_art_and_lyrics();
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.fps = 30;
    app.config.animations.plasma = true;

    let _ = render_app_buffer(&app, 80, 24);
    assert!(app.bridges.canvas_active.get());
    assert!(app.bridges.canvas_heavy_active.get());
    assert_eq!(
        app.animation_draw_fps(),
        20,
        "visible heavy canvas keeps its redraw cap while lyrics are visible"
    );
    assert!(
        app.synchronized_draw_active(),
        "a rendered heavy canvas requests synchronized drawing"
    );
}

#[test]
fn mini_frame_clears_canvas_bridges_and_art_geometry() {
    let mut app = bottom_player_with_art_and_lyrics();
    app.config.animations.master = true;
    app.config.animations.plasma = true;

    let _ = render_app_buffer(&app, 80, 24);
    assert!(app.bridges.canvas_active.get());
    assert!(app.bridges.canvas_heavy_active.get());
    assert!(app.art.rect.get().is_some());

    let _ = render_app_buffer(&app, 28, 8);
    assert_eq!(app.bridges.ui_tier.get(), UiTier::Mini);
    assert!(!app.bridges.canvas_active.get());
    assert!(!app.bridges.canvas_heavy_active.get());
    assert!(app.art.rect.get().is_none());
}

#[test]
fn tiny_bottom_lyrics_hide_focal_only_canvas_without_waking_redraws() {
    let mut app = bottom_player_with_art_and_lyrics();
    app.config.album_art = Some(false);
    app.config.animations.master = true;
    app.config.animations.cube = true;
    app.config.animations.donut = true;

    let buffer = render_app_buffer(&app, 32, 14);
    assert!(buffer_contains(&buffer, LYRIC_MARKER));
    assert!(app.art.rect.get().is_none());
    assert!(!app.bridges.canvas_active.get());
    assert!(!app.bridges.canvas_heavy_active.get());
}

#[test]
fn animations_off_frames_are_stable_across_animation_phase_changes() {
    let _guard = crate::i18n::lock_for_test();
    for position in [PlayerBarPosition::Top, PlayerBarPosition::Bottom] {
        let mut app = app_playing(1, 0);
        app.config.player_bar_position = Some(position);
        app.config.animations.master = false;
        app.playback.paused = true;
        app.playback.time_pos = Some(15.0);
        app.playback.time_pos_at = None;
        app.playback.duration = Some(120.0);

        app.anim.anim_frame = 0;
        let first = render_app_buffer(&app, 100, 30);
        app.anim.anim_frame = 1_337;
        let later = render_app_buffer(&app, 100, 30);

        assert_eq!(
            later, first,
            "animations-off {position:?} output changed with animation phase"
        );
        assert!(!app.bridges.canvas_active.get());
        assert!(!app.bridges.canvas_heavy_active.get());
    }
}

#[test]
fn bottom_field_animations_never_paint_the_docked_bar_or_footer() {
    let _guard = crate::i18n::lock_for_test();
    let make_app = |animations: bool| {
        let mut app = app_playing(1, 0);
        app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
        app.config.album_art = Some(false);
        app.config.animations.master = animations;
        app.config.animations.plasma = animations;
        app.playback.paused = false;
        app.anim.anim_frame = 37;
        app
    };

    let plain = make_app(false);
    let animated = make_app(true);
    let plain_buffer = render_app_buffer(&plain, 80, 24);
    let animated_buffer = render_app_buffer(&animated, 80, 24);
    assert!(animated.bridges.canvas_active.get(), "plasma rendered");

    let inner = Rect::new(1, 1, 78, 22);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(crate::ui::control_box::DOCKED_BOX_ROWS),
        Constraint::Length(1),
    ])
    .split(inner);

    let filler_changed = (rows[1].top()..rows[1].bottom()).any(|y| {
        (rows[1].left()..rows[1].right()).any(|x| {
            plain_buffer.cell((x, y)).expect("plain filler cell")
                != animated_buffer.cell((x, y)).expect("animated filler cell")
        })
    });
    assert!(filler_changed, "plasma must still paint the filler");

    for protected in [rows[2], rows[3]] {
        for y in protected.top()..protected.bottom() {
            for x in protected.left()..protected.right() {
                assert_eq!(
                    plain_buffer.cell((x, y)),
                    animated_buffer.cell((x, y)),
                    "animation leaked into protected docked cell ({x},{y})"
                );
            }
        }
    }
}

#[test]
fn art_protocols_retro_and_popup_stack_render_without_geometry_regressions() {
    let _guard = crate::i18n::lock_for_test();
    for protocol in [
        ProtocolType::Halfblocks,
        ProtocolType::Kitty,
        ProtocolType::Sixel,
        ProtocolType::Iterm2,
    ] {
        let mut app = app_playing(2, 0);
        app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
        app.config.animations.master = true;
        app.config.animations.plasma = true;
        app.queue_popup.open = true;
        make_test_art_active(&mut app, protocol);

        let _ = render_app_buffer(&app, 100, 30);
        assert!(app.art.rect.get().is_some(), "{protocol:?}: final art rect");
        assert!(
            app.queue_popup.rect.get().is_some(),
            "{protocol:?}: queue popup stays in the foreground"
        );
        assert!(app.bridges.canvas_active.get(), "{protocol:?}: canvas ran");
    }

    let mut retro = app_playing(2, 0);
    retro.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    retro.config.retro_mode = true;
    retro.queue_popup.open = true;
    make_test_art_active(&mut retro, ProtocolType::Kitty);
    let _ = render_app_buffer(&retro, 100, 30);
    assert!(retro.art.rect.get().is_some());
    assert!(retro.queue_popup.rect.get().is_some());
}
