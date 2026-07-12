use super::*;

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
    let saved = save_config(&cmds).expect("audius app name change saves config");
    assert_eq!(saved.search.audius_app_name.as_deref(), Some("custom-app"));

    focus_settings_field(&mut app, SettingsTab::General, Field::JamendoClientId);
    app.settings
        .as_mut()
        .unwrap()
        .draft
        .search
        .jamendo_client_id = Some("  ".to_owned());
    let cmds = app.settings_persist_text_field(Field::JamendoClientId);
    assert!(app.config.search.jamendo_client_id.is_none());
    let saved = save_config(&cmds).expect("blank Jamendo client id saves config");
    assert_eq!(saved.search.jamendo_client_id, None);

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
            .any(|c| matches!(c, Cmd::Data(DataCmd::ScanDownloads(path)) if path == &new_dir))
    );
    let saved = save_config(&cmds).expect("download directory change saves config");
    assert_eq!(saved.download_dir.as_deref(), Some(new_dir.as_path()));
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
        draft.spotify_import_mode = crate::config::SpotifyImportMode::FastPlaylist;
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
    change_stored!(SettingsTab::Accounts, Field::SpotifyImportMode);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.spotify_import_mode,
        crate::config::SpotifyImportMode::StrictPlaylist
    );
    change_stored!(SettingsTab::Accounts, Field::ScrobbleLocalFiles);
    assert!(app.settings.as_ref().unwrap().draft.scrobble_local_files);
}

#[test]
fn spotify_picker_keyboard_confirm_maps_all_spotify_imports_to_local_playlists() {
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
                    && matches!(spec.dest, crate::transfer::TransferDest::LocalPlaylist { name: None })
                    && spec.media_kind == crate::transfer::ImportMediaKind::Track
                    && !spec.dry_run
                    && spec.min_score == 0.80
                    && !spec.take_best
                    && spec.auto_accept_ambiguous_min_score == Some(0.75)
        )),
        "liked songs import should target a local library playlist"
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
                    && spec.media_kind == crate::transfer::ImportMediaKind::Track
                    && !spec.dry_run
                    && spec.auto_accept_ambiguous_min_score == Some(0.75)
        )),
        "playlist import should target a local library playlist"
    );
}

#[test]
fn spotify_picker_import_modes_map_to_job_specs() {
    use crate::transfer::actor::PickerPlaylist;

    let mut app = App::new(100);
    app.open_settings();
    let item = PickerPlaylist {
        source: crate::transfer::TransferSource::SpotifyPlaylist {
            id: "pl-1".to_owned(),
        },
        label: "Roadtrip".to_owned(),
        total: 42,
    };

    let confirm_for = |app: &mut App, mode| {
        app.transfer_running = false;
        app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker {
            selected: 0,
            items: vec![item.clone()],
        });
        app.settings.as_mut().unwrap().draft.spotify_import_mode = mode;
        app.spotify_picker_confirm()
    };

    let strict = confirm_for(&mut app, crate::config::SpotifyImportMode::StrictPlaylist);
    assert!(strict.iter().any(|cmd| matches!(
        cmd,
        Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(spec))
            if !spec.dry_run
                && spec.auto_accept_ambiguous_min_score.is_none()
                && spec.media_kind == crate::transfer::ImportMediaKind::Track
                && spec.match_policy == crate::transfer::MatchPolicy::Strict
                && !spec.allow_user_videos
    )));

    let review = confirm_for(&mut app, crate::config::SpotifyImportMode::ReviewFirst);
    assert!(review.iter().any(|cmd| matches!(
        cmd,
        Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(spec))
            if spec.dry_run
                && spec.auto_accept_ambiguous_min_score.is_none()
                && spec.media_kind == crate::transfer::ImportMediaKind::Track
                && spec.match_policy == crate::transfer::MatchPolicy::Strict
                && !spec.allow_user_videos
    )));

    let music_video = confirm_for(
        &mut app,
        crate::config::SpotifyImportMode::MusicVideoPlaylist,
    );
    assert!(music_video.iter().any(|cmd| matches!(
        cmd,
        Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(spec))
            if !spec.dry_run
                && spec.auto_accept_ambiguous_min_score.is_none()
                && spec.media_kind == crate::transfer::ImportMediaKind::MusicVideo
                && spec.match_policy == crate::transfer::MatchPolicy::Strict
                && !spec.take_best
                && !spec.allow_user_videos
    )));
}

#[test]
fn spotify_import_mode_dropdown_keyboard_selects_and_dismisses() {
    let mut app = App::new(100);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::Accounts, Field::SpotifyImportMode);

    let cmds = app.settings_activate();
    assert!(cmds.is_empty());
    assert_eq!(
        app.settings.as_ref().unwrap().spotify_import_mode_dropdown,
        Some(crate::config::SpotifyImportMode::FastPlaylist.index())
    );

    app.on_key_settings(key(KeyCode::Down));
    assert_eq!(
        app.settings.as_ref().unwrap().spotify_import_mode_dropdown,
        Some(crate::config::SpotifyImportMode::StrictPlaylist.index())
    );
    app.on_key_settings(key(KeyCode::Enter));
    let st = app.settings.as_ref().unwrap();
    assert_eq!(
        st.draft.spotify_import_mode,
        crate::config::SpotifyImportMode::StrictPlaylist
    );
    assert!(st.spotify_import_mode_dropdown.is_none());

    app.settings_activate();
    app.on_key_settings(key(KeyCode::Down));
    app.on_key_settings(key(KeyCode::Down));
    assert_eq!(
        app.settings.as_ref().unwrap().spotify_import_mode_dropdown,
        Some(crate::config::SpotifyImportMode::MusicVideoPlaylist.index())
    );
    app.on_key_settings(key(KeyCode::Enter));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.spotify_import_mode,
        crate::config::SpotifyImportMode::MusicVideoPlaylist
    );

    app.settings_activate();
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .spotify_import_mode_dropdown
            .is_some()
    );
    app.on_key_settings(key(KeyCode::Esc));
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .spotify_import_mode_dropdown
            .is_none()
    );
}

#[test]
fn spotify_import_mode_dropdown_mouse_targets_select_and_dismiss() {
    let mut app = App::new(100);
    app.mode = Mode::Settings;
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::Accounts, Field::SpotifyImportMode);

    let _ = render_app_buffer(&app, 100, 32);
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::SettingsSpotifyImportModeMenu)
    );

    let cmds = click_target(&mut app, MouseTarget::SettingsSpotifyImportModeMenu);
    assert!(cmds.is_empty());
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .spotify_import_mode_dropdown
            .is_some()
    );

    let _ = render_app_buffer(&app, 100, 32);
    for mode in crate::config::SpotifyImportMode::ALL {
        assert!(
            app.hits
                .regions()
                .iter()
                .any(|r| r.target == MouseTarget::SettingsSpotifyImportModeSelect(mode)),
            "missing dropdown hit rect for {mode:?}"
        );
    }

    let cmds = click_target(
        &mut app,
        MouseTarget::SettingsSpotifyImportModeSelect(
            crate::config::SpotifyImportMode::MusicVideoPlaylist,
        ),
    );
    assert!(cmds.is_empty());
    let st = app.settings.as_ref().unwrap();
    assert_eq!(
        st.draft.spotify_import_mode,
        crate::config::SpotifyImportMode::MusicVideoPlaylist
    );
    assert!(st.spotify_import_mode_dropdown.is_none());

    let cmds = click_target(&mut app, MouseTarget::SettingsSpotifyImportModeMenu);
    assert!(cmds.is_empty());
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .spotify_import_mode_dropdown
            .is_some()
    );
    let _ = render_app_buffer(&app, 100, 32);
    let other_control = app
        .hits
        .regions()
        .iter()
        .find(|r| r.target == (MouseTarget::SettingsChange { row: 0, delta: 1 }))
        .map(|r| (r.rect.x + r.rect.width / 2, r.rect.y + r.rect.height / 2))
        .expect("outside settings control hit rect");
    let cmds = app.update(Msg::MouseClick {
        col: other_control.0,
        row: other_control.1,
        multi: false,
    });
    assert!(cmds.is_empty());
    let st = app.settings.as_ref().unwrap();
    assert!(st.spotify_import_mode_dropdown.is_none());
    assert_eq!(
        st.draft.spotify_import_mode,
        crate::config::SpotifyImportMode::MusicVideoPlaylist,
        "outside click should dismiss without applying the other setting"
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
        auto_accepted: 0,
        ambiguous: 1,
        not_found: 0,
        written: 0,
        current: "Artist - Song".to_owned(),
    })));
    assert!(app.transfer_running);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Spotify import: matching 2/5"));
    assert!(app.status.text.contains("matched 1"));
    assert!(app.status.text.contains("review 1"));
    assert!(app.status.text.contains("Artist - Song"));

    app.transfer_running = true;
    app.update(Msg::Transfer(TransferEvent::JobDone(Box::new(
        crate::transfer::checkpoint::TransferReport {
            job_id: "sp2yt-1".to_owned(),
            total: 5,
            matched: 4,
            written: 4,
            ..Default::default()
        },
    ))));
    assert!(!app.transfer_running);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("Import finished"));
    assert!(app.status.text.contains("ytt transfer session sp2yt-1"));
    assert!(app.status.text.contains("Library > Playlists"));
    assert!(!app.status.text.contains("Shift+D"));
    assert!(app.status.text.contains("Import Sessions"));

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
