use super::*;

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
fn volume_flash_geometry_is_stable_and_off_mode_keeps_legacy_layout() {
    // This pins the LEGACY Top-layout bytes/geometry, so opt out of the docked default.
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    let targets = [
        MouseTarget::Player(Action::PrevTrack),
        MouseTarget::Player(Action::TogglePause),
        MouseTarget::Player(Action::NextTrack),
        MouseTarget::Player(Action::VolDown),
        MouseTarget::Player(Action::VolUp),
        MouseTarget::VolumeArea,
    ];
    let geometry = |app: &App| {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        targets
            .iter()
            .map(|target| {
                app.hits
                    .regions()
                    .iter()
                    .find(|region| &region.target == target)
                    .unwrap_or_else(|| panic!("missing hit target {target:?}"))
                    .rect
            })
            .collect::<Vec<_>>()
    };

    app.playback.volume = 9;
    let legacy_narrow = geometry(&app);
    app.playback.volume = 100;
    let legacy_wide = geometry(&app);
    let control_row = |app: &App| {
        let buf = render_app_buffer(app, 80, 20);
        (0..80).map(|x| buf[(x, 6)].symbol()).collect::<String>()
    };
    app.playback.volume = 9;
    assert_eq!(
        control_row(&app),
        "│                        ⇤     ‖     ⇥       vol  - 9% +                       │"
    );
    app.playback.volume = 100;
    assert_eq!(
        control_row(&app),
        "│                       ⇤     ‖     ⇥       vol  - 100% +                      │"
    );
    assert_ne!(legacy_narrow, legacy_wide, "legacy hit geometry changed");

    app.config.animations.master = true;
    app.config.animations.volume_flash = true;
    app.playback.volume = 50;
    let expected = geometry(&app);
    for volume in [0, 9, 10, 99, 100] {
        app.playback.volume = volume;
        assert_eq!(geometry(&app), expected, "control strip moved at {volume}%");
    }
}

#[test]
fn beginner_control_row_tracks_volume_rebinds_and_responsive_fallbacks() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(2, 0);
    app.config.beginner_mode = true;
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    let buf = render_app_buffer(&app, 80, 20);
    let row = (0..80).map(|x| buf[(x, 6)].symbol()).collect::<String>();
    assert!(row.contains("Volume  [↓] - 100% + [↑]"), "got {row:?}");
    assert!(
        app.hits
            .rect_of_target(MouseTarget::VolumeArea)
            .is_some_and(|rect| rect.width > 0)
    );

    // The Player block contributes two border cells, leaving a 35-cell control area.
    let narrow = render_app_buffer(&app, 37, 20);
    let narrow_row = (0..37).map(|x| narrow[(x, 6)].symbol()).collect::<String>();
    assert!(
        narrow_row.contains("vol  [↓] - 100% + [↑]"),
        "got {narrow_row:?}"
    );

    app.keymap
        .rebind(
            KeyContext::Player,
            Action::VolDown,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();
    app.keymap
        .rebind(
            KeyContext::Player,
            Action::VolUp,
            crate::keymap::parse_chord("f9").unwrap(),
        )
        .unwrap();
    let rebound = render_app_buffer(&app, 80, 20);
    let rebound_row = (0..80)
        .map(|x| rebound[(x, 6)].symbol())
        .collect::<String>();
    assert!(
        rebound_row.contains("Volume  [F8] - 100% + [F9]"),
        "got {rebound_row:?}"
    );

    let mini = render_app_buffer(&app, 28, 8);
    let mini_row = (0..28).map(|x| mini[(x, 2)].symbol()).collect::<String>();
    assert!(mini_row.contains("vol  - 100% +"), "got {mini_row:?}");
    assert!(!mini_row.contains("[F"), "got {mini_row:?}");

    app.keymap.unbind(KeyContext::Player, Action::VolDown);
    let unbound = render_app_buffer(&app, 80, 20);
    let unbound_row = (0..80)
        .map(|x| unbound[(x, 6)].symbol())
        .collect::<String>();
    assert!(
        unbound_row.contains("Volume  - 100% + [F9]"),
        "got {unbound_row:?}"
    );
    assert!(!unbound_row.contains("[F8]"), "got {unbound_row:?}");
}

#[test]
fn beginner_status_buttons_use_accent_bold_without_changing_off_mode() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.config.beginner_mode = true;
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    let beginner = render_app_buffer(&app, 100, 20);
    let rect = app
        .hits
        .rect_of_target(MouseTarget::Player(Action::ToggleShuffle))
        .expect("beginner Shuffle hit target");
    let cell = beginner
        .cell((rect.x, rect.y))
        .expect("Shuffle starts inside the buffer");
    assert_eq!(cell.fg, app.theme.color(crate::theme::ThemeRole::Accent));
    assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));

    app.config.beginner_mode = false;
    let legacy = render_app_buffer(&app, 100, 20);
    let rect = app
        .hits
        .rect_of_target(MouseTarget::Player(Action::ToggleShuffle))
        .expect("legacy shuffle hit target");
    let cell = legacy
        .cell((rect.x, rect.y))
        .expect("shuffle starts inside the buffer");
    assert_eq!(
        cell.fg,
        app.theme.color(crate::theme::ThemeRole::PlayerLabel)
    );
    assert!(!cell.modifier.contains(ratatui::style::Modifier::BOLD));
}

#[test]
fn beginner_coach_actions_fit_the_full_minimum_and_mini_only_offers_skip() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = true;
    app.prepare_beginner_onboarding(true);

    let _ = render_app_buffer(&app, 32, 14);
    let full_regions = app
        .hits
        .regions()
        .iter()
        .filter(|region| matches!(region.target, MouseTarget::Onboarding(_)))
        .cloned()
        .collect::<Vec<_>>();
    for action in [
        OnboardingAction::Noop,
        OnboardingAction::Back,
        OnboardingAction::Primary,
        OnboardingAction::Skip,
    ] {
        assert!(
            full_regions
                .iter()
                .any(|region| region.target == MouseTarget::Onboarding(action)),
            "minimum full layout omitted {action:?}: {full_regions:?}"
        );
    }
    assert!(full_regions.iter().all(|region| {
        let rect = region.rect;
        rect.width > 0 && rect.height > 0 && rect.right() <= 32 && rect.bottom() <= 14
    }));

    let _ = render_app_buffer(&app, 31, 14);
    let mini_actions = app
        .hits
        .regions()
        .iter()
        .filter_map(|region| match region.target {
            MouseTarget::Onboarding(OnboardingAction::Noop) => None,
            MouseTarget::Onboarding(action) => Some(action),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(mini_actions, vec![OnboardingAction::Skip]);
}

#[test]
fn minimum_full_finish_primary_turns_beginner_mode_off_for_mouse_users() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = true;
    app.config.beginner_tutorial.next_step = "finish".to_owned();
    app.prepare_beginner_onboarding(true);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
    let _ = render_app_buffer(&app, 32, 14);
    let button = app
        .hits
        .regions()
        .iter()
        .find(|region| region.target == MouseTarget::Onboarding(OnboardingAction::Primary))
        .cloned()
        .expect("Finish primary button");

    app.on_mouse_click(button.rect.x, button.rect.y, false);
    assert!(!app.settings.as_ref().unwrap().draft.beginner_mode);
    assert!(app.onboarding.active(), "saving is still required");
}

#[test]
fn rendering_settings_registers_clickable_controls() {
    // Each control kind must publish its own hit target *on top of* the row-select rect, so a
    // click changes/activates the value rather than only moving the cursor onto it.
    let render_targets = |tab: SettingsTab| -> Vec<MouseTarget> {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (mode → Settings)
        app.settings.as_mut().unwrap().tab = tab;
        // Tall enough for every General row with the docked player bar reserving 5 rows.
        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
        app.hits
            .regions()
            .iter()
            .map(|b| b.target.clone())
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
        has(&g, MouseTarget::SettingsColorSwatch(3)),
        "color swatch opens picker"
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

    // General's non-destructive export and destructive Reset buttons activate on click.
    let general = render_targets(SettingsTab::General);
    let export = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ExportPersonalData)
        .unwrap();
    let reset_all = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::ResetAll)
        .unwrap();
    assert!(
        has(&general, MouseTarget::SettingsActivate(export)),
        "personal-data export button"
    );
    assert!(
        has(&general, MouseTarget::SettingsActivate(reset_all)),
        "reset-all button"
    );
}

#[test]
fn exporting_personal_data_disables_its_mouse_action_without_breaking_narrow_rendering() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let export = SettingsTab::General
        .fields()
        .iter()
        .position(|field| *field == Field::ExportPersonalData)
        .unwrap();
    {
        let st = app.settings.as_mut().unwrap();
        st.row = export;
        st.personal_data_export = crate::settings::PersonalDataExportStatus::Exporting;
    }

    const WIDTH: u16 = 48;
    // Keep the height above the dedicated mini-player tier; this regression is about horizontal
    // clipping and the busy button's hit target, not the intentionally player-only mini layout.
    let backend = TestBackend::new(WIDTH, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    assert!(buffer_contains(&buffer, "Exporting…"));
    assert!(
        app.hits
            .regions()
            .iter()
            .all(|button| button.target != MouseTarget::SettingsActivate(export)),
        "busy export must not publish a second activation target"
    );
}

#[test]
fn personal_data_export_row_renders_idle_and_result_states() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let export = SettingsTab::General
        .fields()
        .iter()
        .position(|field| *field == Field::ExportPersonalData)
        .unwrap();
    app.settings.as_mut().unwrap().row = export;

    let warning = render_app_buffer(&app, 80, 32);
    assert!(buffer_contains(
        &warning,
        "unencrypted JSON · includes private listening history"
    ));

    for (status, text) in [
        (
            crate::settings::PersonalDataExportStatus::Idle,
            "↵ Export to Downloads",
        ),
        (
            crate::settings::PersonalDataExportStatus::Succeeded,
            "✓ Exported",
        ),
        (
            crate::settings::PersonalDataExportStatus::Failed,
            "Failed · ↵ retry",
        ),
    ] {
        app.settings.as_mut().unwrap().personal_data_export = status;
        let buffer = render_app_buffer(&app, 80, 32);
        assert!(
            buffer_contains(&buffer, text),
            "missing export state: {text}"
        );
    }
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
    assert!(
        app.update(Msg::MouseClick {
            col: 32,
            row: 4,
            multi: false
        })
        .is_empty()
    );
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
    assert!(
        app.update(Msg::MouseClick {
            col: 32,
            row: 4,
            multi: false
        })
        .is_empty()
    );
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
    let cmds = app.update(Msg::MouseClick {
        col: 33,
        row: 6,
        multi: false,
    });
    assert_eq!(app.audio.preset, EqPreset::Flat);
    assert!(app.dropdowns.eq_open, "dropdown waits for admission");
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("equalizer"))
    ));
    app.admit_player_intents_for_test(&cmds);
    assert_eq!(app.audio.preset, EqPreset::Vocal);
    assert_eq!(app.audio.bands, EqPreset::Vocal.gains());
    assert!(!app.dropdowns.eq_open);
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
    let cmds = app.update(Msg::MouseClick {
        col: 50,
        row: 5,
        multi: false,
    });
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
    app.show_tool_setup(ToolSetupContext::Startup, vec!["mpv"]);
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_TOOL_SETUP_BIT);
    app.tool_setup = None;
    app.config.beginner_mode = true;
    app.prepare_beginner_onboarding(true);
    assert!(app.onboarding.visible());
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_BEGINNER_BIT);
    app.onboarding = OnboardingState::default();
    let _ = app.open_audio_output_picker();
    assert_eq!(app.art_overlay_mask(), ART_OVERLAY_AUDIO_OUTPUT_BIT);
    app.close_audio_output_picker();
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
