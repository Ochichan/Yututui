use super::*;

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
    assert_opaque_rect(&buf, centered_fixed(modal_area, 60, 25));

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
    let popup = centered_fixed(area, 60, 25);
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
fn ai_empty_state_while_playing_renders_groove_frame() {
    let mut app = app_playing(1, 0);
    app.mode = Mode::Ai;
    app.config.animations.master = true;
    // Pin the tick rate: frame_index_for_tick(10, fps=30) must land on the cat
    // asset's frame 1 regardless of future FPS_DEFAULT changes.
    app.config.animations.fps = 30;
    app.anim.anim_frame = 10;

    assert!(app.ai_mascot_active());

    let buf = render_app_buffer(&app, 100, 30);
    assert!(
        buffer_contains(&buf, "⠩⠳⢯⣀⣴⠟"),
        "playing AI empty state should render a non-idle groove pose"
    );
    assert!(
        buffer_contains(&buf, "⢻⣶⠤⣄"),
        "groove pose should be the cat_laptop mascot"
    );
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
