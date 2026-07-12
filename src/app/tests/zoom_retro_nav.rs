use super::*;

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
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);

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

#[test]
fn narrow_nav_pages_with_arrows_so_every_screen_stays_clickable() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.mode = Mode::Library;

    // Keep the width narrow enough to page while staying at the Full/Mini height boundary;
    // shorter frames intentionally render the miniplayer and have no screen nav.
    let (targets, top) = render_at(&app, 40, crate::ui::layout::MINI_MIN_H);
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
    render_at(&app, 40, crate::ui::layout::MINI_MIN_H);

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
        multi: false,
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
    let (_, top) = render_at(&app, 40, crate::ui::layout::MINI_MIN_H);
    assert!(
        top.contains('◁') && !top.contains('◀'),
        "first tab: left arrow should be hollow: {top:?}"
    );
    assert!(top.contains('▶'), "right arrow stays filled: {top:?}");

    // Last tab (DJ Gem): can't go right — the right arrow renders hollow.
    app.mode = Mode::Ai;
    let (_, top) = render_at(&app, 40, crate::ui::layout::MINI_MIN_H);
    assert!(
        top.contains('▷') && !top.contains('▶'),
        "last tab: right arrow should be hollow: {top:?}"
    );
    assert!(top.contains('◀'), "left arrow stays filled: {top:?}");
}

#[test]
fn ai_model_label_stays_off_nav_border_when_narrow() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    // Pin the model so the label under test is stable. The label now lives below the prompt,
    // so no part of it should survive on the nav border at any width.
    app.ai.model = crate::ai::GeminiModel::Latest;

    let (targets, top) = render_at(&app, 100, 30);
    assert!(
        !top.contains("Latest"),
        "wide: the model label should not be on the nav border: {top:?}"
    );
    assert!(
        targets.contains(&MouseTarget::AiModel),
        "wide: the model label should remain clickable below the prompt"
    );

    let (_, top) = render_at(&app, 30, 30);
    assert!(
        !top.contains("Latest") && !top.contains("est"),
        "narrow: no model label and no clipped remnant on the nav border: {top:?}"
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
