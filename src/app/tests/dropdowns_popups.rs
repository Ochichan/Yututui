use super::*;

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
fn radio_and_local_confirm_buttons_have_taller_click_targets() {
    let mut app = App::new(100);
    app.radio_mode.pending_radio_mode_confirm = Some(RadioModeConfirm::Enter);
    render_app(&app);
    assert_tall_hit(&app, MouseTarget::ConfirmRadioMode);
    assert_tall_hit(&app, MouseTarget::CancelRadioMode);

    app.radio_mode.pending_radio_mode_confirm = None;
    app.local_mode.pending_confirm = Some(LocalModeConfirm::Enter);
    render_app(&app);
    assert_tall_hit(&app, MouseTarget::ConfirmLocalMode);
    assert_tall_hit(&app, MouseTarget::CancelLocalMode);
}

#[test]
fn local_organize_confirm_buttons_have_taller_click_targets() {
    let mut app = App::new(100);
    app.local_mode.pending_organize_confirm = Some(LocalOrganizeConfirm {
        session_id: "sp2yt-session".to_owned(),
        root: std::path::PathBuf::from("/tmp/music"),
        move_count: 2,
        already_count: 1,
        skipped_count: 0,
    });
    render_app(&app);
    assert_tall_hit(&app, MouseTarget::ConfirmLocalOrganize);
    assert_tall_hit(&app, MouseTarget::CancelLocalOrganize);
}

fn assert_tall_hit(app: &App, target: MouseTarget) {
    let rect = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == target)
        .map(|b| b.rect)
        .unwrap_or_else(|| panic!("missing hit rect for {target:?}"));
    assert!(
        rect.height >= 2,
        "{target:?} should have a taller click target, got {rect:?}"
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
            automatic_final_dir: None,
            close_barrier: None,
            save_request: None,
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
