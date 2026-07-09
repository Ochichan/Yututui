use super::*;

fn focus_current_settings_field(app: &mut App, field: Field) {
    let row = app
        .settings
        .as_ref()
        .expect("settings open")
        .fields()
        .iter()
        .position(|f| *f == field)
        .expect("field exists in current settings tab");
    for _ in 0..row {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
}

#[test]
fn settings_key_opens_and_q_closes_without_quitting() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Accounts);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General); // wraps
}

#[test]
fn settings_accounts_tab_renders_service_sections() {
    let mut app = App::new(100);
    app.config.retro_mode = true; // English labels for stable assertions
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    app.settings.as_mut().unwrap().tab = SettingsTab::Accounts;

    let buf = render_app_buffer(&app, 120, 40);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(text.contains("Last.fm"), "Last.fm section header renders");
    assert!(
        text.contains("ListenBrainz"),
        "ListenBrainz section renders"
    );
    assert!(text.contains("Spotify"), "Spotify section renders");
    assert!(
        text.contains("connect in browser"),
        "disconnected accounts offer the connect action"
    );
    assert!(
        text.contains("Client ID"),
        "Spotify Client ID field renders"
    );
}

#[test]
fn settings_whole_row_click_activates_a_button_row() {
    // B2: a click anywhere on a Button row (not only the value glyph) activates it. With no
    // Client ID set, activating the Spotify "connect" row surfaces the guidance message — proof
    // the click reached the handler rather than only focusing the row.
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.config.spotify.client_id = None; // force the empty-Client-ID connect path
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.settings.as_mut().unwrap().tab = SettingsTab::Accounts;
    let idx = app
        .settings
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| *f == Field::SpotifyConnect)
        .expect("a SpotifyConnect row");
    assert!(app.status.text.is_empty());
    let _ = app.on_list_row_click(idx);
    assert_eq!(
        app.settings.as_ref().unwrap().row,
        idx,
        "the row is focused"
    );
    // A whole-row click on a Button row activates it — the connect handler always sets a status
    // (empty-ID guidance, or a reconnect notice if a token is present). A focus-only click, the
    // old behaviour, would leave the status empty.
    assert!(
        !app.status.text.is_empty(),
        "a whole-row click activated the button (status still empty)"
    );
}

#[test]
fn spotify_picker_click_selects_then_confirms() {
    // C: the first click on a picker row selects it (no job yet); clicking the already-selected
    // row imports it — closing the picker and dispatching a transfer.
    use crate::transfer::actor::PickerPlaylist;
    let mut app = App::new(100);
    let items = vec![
        PickerPlaylist {
            source: crate::transfer::TransferSource::SpotifyLiked,
            label: "Liked Songs".to_owned(),
            total: 0,
        },
        PickerPlaylist {
            source: crate::transfer::TransferSource::SpotifyPlaylist {
                id: "abc".to_owned(),
            },
            label: "Roadtrip".to_owned(),
            total: 12,
        },
    ];
    app.overlays.spotify_picker = Some(crate::app::state::SpotifyPicker { items, selected: 0 });

    let cmds = app.on_mouse_target(MouseTarget::SpotifyPickRow(1));
    assert!(cmds.is_empty(), "selecting a new row doesn't start a job");
    assert_eq!(
        app.overlays.spotify_picker.as_ref().unwrap().selected,
        1,
        "the clicked row becomes selected"
    );

    let cmds = app.on_mouse_target(MouseTarget::SpotifyPickRow(1));
    assert!(
        app.overlays.spotify_picker.is_none(),
        "clicking the selected row closes the picker"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::Transfer(_))),
        "confirming dispatches a transfer job"
    );
}

#[test]
fn settings_keys_lists_radio_normal_mode_binding() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    app.settings.as_mut().unwrap().tab = SettingsTab::Keys;

    let buf = render_app_buffer(&app, 120, 40);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("Player"),
        "Keys tab should show player bindings"
    );
    assert!(
        text.contains("Radio/Normal mode"),
        "Keys tab should list the mode-switch action"
    );
    assert!(
        text.contains("Alt+Shift+R"),
        "Keys tab should show the default mode-switch key"
    );
    assert!(
        text.contains("Enter / exit Local Deck"),
        "Keys tab should list the Local Deck mode binding"
    );
    assert!(
        text.contains("Alt+Shift+L"),
        "Keys tab should show the default Local Deck key"
    );
}

#[test]
fn help_overlay_shows_player_radio_normal_mode_binding() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.overlays.help_visible = true;

    let buf = render_app_buffer(&app, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("Radio/Normal mode"),
        "Help should show the player-only mode-switch action"
    );
    assert!(
        text.contains("Alt+Shift+R"),
        "Help should show the default mode-switch key"
    );
}

#[test]
fn retro_frames_contain_only_cp437_safe_cells() {
    // The whole point of retro mode: whatever the app renders — CJK metadata, emoji
    // toggles, braille art, animation glyphs, the About icon — the scrubbed frame must
    // contain nothing a 256-glyph console font can't show.
    let mut app = App::new(100);
    app.queue.set(
        vec![crate::api::Song::remote(
            "vid1",
            "한글 제목 ♫",
            "アーティスト",
            "3:00",
        )],
        0,
    );
    app.mode = Mode::Player;
    app.config.retro_mode = true;
    app.queue.shuffle = true;
    app.queue.repeat = crate::queue::Repeat::One;
    app.config.animations.master = true;
    app.config.animations.spinner = true;
    app.config.animations.eq_bars = true;
    // Album art active (halfblocks fallback picker): retro must render it as ASCII art.
    configure_test_art_picker(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);
    app.set_artwork(
        "vid1".to_owned(),
        Some(image::DynamicImage::new_rgb8(64, 64)),
    );

    let assert_scrubbed = |app: &App, label: &str| {
        let buf = render_app_buffer(app, 100, 30);
        for (i, cell) in buf.content().iter().enumerate() {
            assert!(
                crate::ui::retro::retro_supported(cell.symbol()),
                "{label}: cell {i} holds unsupported symbol {:?}",
                cell.symbol()
            );
        }
    };

    assert_scrubbed(&app, "player");
    app.overlays.about_visible = true;
    assert_scrubbed(&app, "about card");
    app.overlays.about_visible = false;
    app.overlays.help_visible = true;
    assert_scrubbed(&app, "help overlay");
    app.overlays.help_visible = false;
    app.mode = Mode::Search;
    assert_scrubbed(&app, "search");
    app.mode = Mode::Library;
    assert_scrubbed(&app, "library");
    app.mode = Mode::Player;
    app.radio_dedicated_mode = true;
    assert_scrubbed(&app, "radio mode");
}

#[test]
fn remapped_focus_keys_switch_library_and_settings_tabs() {
    let mut app = app_playing(1, 0);
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::FocusNext,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::FocusPrev,
            crate::keymap::parse_chord("f6").unwrap(),
        )
        .unwrap();

    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app.update(Msg::Key(key(KeyCode::F(6))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::All);

    app.update(Msg::Key(key(KeyCode::Char('q'))));
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Playback);
    app.update(Msg::Key(key(KeyCode::F(6))));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
}

#[test]
fn transient_status_expires_after_ttl_and_restores_the_title() {
    let mut app = app_playing(1, 0);
    // A notification covers the title and arms the expiry timer.
    app.update(Msg::Key(key(KeyCode::Char('N')))); // toggle normalize → sets status
    assert!(!app.status.text.is_empty(), "an action should set a status");
    assert!(
        app.status_visible(),
        "a non-empty status arms the expiry tick"
    );

    // Before the TTL elapses, a tick is a no-op — the notification stays.
    app.update(Msg::StatusTick);
    assert!(
        !app.status.text.is_empty(),
        "status persists until the TTL elapses"
    );
    assert!(app.status_visible());

    // Backdate the timer past the TTL; the next tick clears it and restores the title.
    app.status.set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    app.dirty = false; // so the assertion below proves the clear requested the redraw
    app.update(Msg::StatusTick);
    assert!(
        app.status.text.is_empty(),
        "status auto-clears after the TTL"
    );
    assert!(!app.status_visible(), "expiry disarms the tick");
    assert!(
        app.dirty,
        "clearing the status requests a redraw of the title"
    );
}

#[test]
fn streaming_mode_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    use crate::streaming::StreamingMode;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Fields: AiEnabled(0), Model(1), ApiKey(2), ReplyLanguage(3), RomanizedTitles(4),
    // Clear cache(5), AutoplayStreaming(6), CuratingMode(7), StreamingMode(8).
    for _ in 0..8 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right))); // Balanced → Discovery
    assert_eq!(
        app.settings.as_ref().unwrap().draft.streaming_mode,
        StreamingMode::Discovery
    );
    assert!(app.status.text.contains("Curating style: Discovery"));
    // Closing settings commits the draft into config + emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.streaming.mode, StreamingMode::Discovery);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn curating_mode_cycles_on_the_ai_tab_and_persists_to_ai_enabled() {
    let _guard = crate::i18n::lock_for_test();
    use crate::streaming::CuratingMode;
    let mut app = app_playing(1, 0);
    assert!(app.config.streaming.ai.enabled); // default → DJ Gem
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    // Down to CuratingMode (index 7), then step it: DJ Gem → YT Native.
    for _ in 0..7 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.curating_mode,
        CuratingMode::YtNative
    );
    assert!(app.status.text.contains("Curating mode:"));
    // Close → the AI rerank flag is now off.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.config.streaming.ai.enabled);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn dj_gem_reply_language_cycles_on_the_ai_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test(); // English UI
    use crate::i18n::DjGemLanguage;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
    // Down to Reply language (index 3: AiEnabled, Model, ApiKey, ReplyLanguage).
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::DjGemLanguage)
    );
    app.update(Msg::Key(key(KeyCode::Right))); // Auto → English (CYCLE order)
    assert_eq!(
        app.settings.as_ref().unwrap().draft.dj_gem_language,
        DjGemLanguage::English
    );
    assert!(app.status.text.contains("Reply language:"));
    // Closing commits the pick into config and emits a save.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.dj_gem_language, DjGemLanguage::English);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn dj_gem_reply_language_is_locked_to_english_under_retro() {
    let _guard = crate::i18n::lock_for_test();
    use crate::i18n::DjGemLanguage;
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab)));
    }
    app.settings.as_mut().unwrap().draft.retro_mode = true;
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::DjGemLanguage)
    );
    app.update(Msg::Key(key(KeyCode::Right)));
    // The underlying pick is untouched (still Auto) and the row explains the lock.
    assert_eq!(
        app.settings.as_ref().unwrap().draft.dj_gem_language,
        DjGemLanguage::Auto
    );
    assert!(app.status.text.contains("Retro mode replies in English"));
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .value_display(Field::DjGemLanguage),
        "English (Retro mode)"
    );
}

#[test]
fn streaming_source_cycles_on_general_tab_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    // Fields: Language(0), SearchSource(1), StreamingSource(2).
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Right))); // YouTube -> SoundCloud
    assert_eq!(
        app.settings.as_ref().unwrap().draft.search.streaming_source,
        SearchSource::SoundCloud
    );
    assert!(app.status.text.contains("Streaming source: SoundCloud"));

    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.config.search.streaming_source, SearchSource::SoundCloud);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn clear_romanized_title_cache_button_is_hidden_in_retro_draft() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }

    let st = app.settings.as_ref().unwrap();
    assert_eq!(st.tab, SettingsTab::Ai);
    assert!(st.fields().contains(&Field::ClearRomanizedTitleCache));

    app.settings.as_mut().unwrap().draft.retro_mode = true;
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .fields()
            .contains(&Field::ClearRomanizedTitleCache)
    );
}

#[test]
fn clear_romanized_title_cache_confirms_and_discards_stale_results() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.config.romanized_titles = Some(true);
    app.romanization.next_request_id = 7;
    let song = Song::remote("ko1", "좋은 날", "아이유", "0:10");
    let stale_key = crate::romanize::key_for_song(&song);
    assert!(app.romanization.cache.ensure_local(&song));
    assert!(app.romanization.cache.entry_for(&song).is_some());

    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab
    }
    let idx = app
        .settings
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| *f == Field::ClearRomanizedTitleCache)
        .expect("clear cache field");
    app.settings.as_mut().unwrap().row = idx;

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ClearRomanizedTitleCache)
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert_eq!(app.status.text, "Romanized title cache cleared");
    assert!(app.romanization.cache.entry_for(&song).is_none());
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::ClearRomanizedTitles)))
    );

    let cmds = app.apply_romanized_titles(
        7,
        vec![stale_key.clone()],
        vec![crate::romanize::RomanizedResult {
            key: stale_key,
            title: "Joeun Nal".to_owned(),
            artist: "IU".to_owned(),
            confidence: Some(0.9),
        }],
    );
    assert!(cmds.is_empty());
    assert!(app.romanization.cache.entry_for(&song).is_none());
}

#[test]
fn settings_key_capture_accepts_ctrl_chords() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Hotkeys tab (index 2)
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
    app.update(Msg::Key(key(KeyCode::Enter))); // capture player.toggle_pause

    // `q` is already Back in Player → a conflict warning pops instead of silently
    // dropping the rebind, and it names the offending chord, action, and context.
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    let conflict = app
        .overlays
        .key_conflict
        .expect("a conflict warning should be raised");
    assert_eq!(conflict.existing, Action::Back);
    assert_eq!(conflict.ctx, KeyContext::Player);
    assert_eq!(conflict.chord, crate::keymap::parse_chord("q").unwrap());
    // The binding was left untouched: space still toggles pause, `q` still means Back.
    let km = &app.settings.as_ref().unwrap().keymap;
    assert_eq!(
        km.action(
            KeyContext::Player,
            crate::keymap::parse_chord("space").unwrap()
        ),
        Some(Action::TogglePause)
    );
    assert_eq!(
        km.action(KeyContext::Player, crate::keymap::parse_chord("q").unwrap()),
        Some(Action::Back)
    );

    // The popup is modal: the next key only dismisses it (here `q` does NOT save+quit).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(app.overlays.key_conflict.is_none());
    assert!(
        save_config(&cmds).is_none(),
        "dismiss key must be swallowed, not saved"
    );
    assert!(app.settings.is_some(), "settings stayed open after dismiss");
}

#[test]
fn settings_mpv_overlay_key_capture_rejects_unsupported_keys() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    let row = crate::keymap::editable_entries()
        .iter()
        .position(|entry| *entry == (KeyContext::MpvOverlay, Action::VideoTogglePause))
        .expect("mpv overlay pause binding");
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = SettingsTab::Keys;
        st.row = row;
    }

    app.settings_begin_capture();
    assert_eq!(
        app.settings.as_ref().unwrap().capturing,
        Some((KeyContext::MpvOverlay, Action::VideoTogglePause))
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Media(
        crossterm::event::MediaKeyCode::PlayPause,
    ))));

    assert!(cmds.is_empty());
    assert!(app.status.text.contains("mpv"));
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .keymap
            .chord(KeyContext::MpvOverlay, Action::VideoTogglePause),
        crate::keymap::parse_chord("space")
    );
}

/// Move the General-tab cursor onto the Reset-all button.

#[test]
fn reset_keybindings_button_restores_defaults_and_persists_on_close() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(1, 0);
    app.keymap
        .rebind(
            KeyContext::Player,
            Action::TogglePause,
            crate::keymap::parse_chord("x").unwrap(),
        )
        .unwrap();
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
        Some(Action::TogglePause)
    );

    focus_reset_keybindings(&mut app);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ResetKeybindings)
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
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
        draft_keymap.action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
        None
    );
    // The live keymap follows the existing Settings flow: changes commit on close.
    assert_eq!(
        app.keymap
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
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
            .action(KeyContext::Player, crate::keymap::parse_chord("x").unwrap()),
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
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
    // `y` confirms → every draft value is back to its default.
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(app.overlays.pending_settings_confirm.is_none());
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
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(SettingsConfirm::ResetAll)
    );
    app.update(Msg::Key(key(KeyCode::Esc))); // anything but Enter/`y` cancels
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!((app.settings.as_ref().unwrap().draft.speed - 1.8).abs() < 1e-9);
}

#[test]
fn settings_theme_persists_when_closed_with_back() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → Graphics tab (index 3); row 0 = ThemePreset
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Graphics);

    app.update(Msg::Key(key(KeyCode::Down))); // row 1 = ThemePreset
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    let role = crate::theme::ThemeRole::Accent;
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = SettingsTab::Graphics;
        // ThemeColor rows start at field index 3 (after RetroMode, ThemePreset, BackgroundNone).
        st.row = 3 + crate::theme::ThemeRole::ALL
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General)
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab; row 0 = Speed
    app.update(Msg::Key(key(KeyCode::Right))); // speed 1.0 -> 1.1 (draft)
    assert!(
        (app.playback.speed - 1.0).abs() < 1e-9,
        "committed speed unchanged while editing"
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('q')))); // save+quit
    assert_eq!(app.mode, Mode::Player);
    assert!(
        (app.playback.speed - 1.1).abs() < 1e-9,
        "speed applied on close"
    );
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.speed, Some(1.1));
}

#[test]
fn settings_close_persists_live_audio() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback; Speed
    app.update(Msg::Key(key(KeyCode::Right))); // draft speed -> 1.1
    let cmds = app.update(Msg::Key(key(KeyCode::Esc))); // save+quit
    assert_eq!(app.mode, Mode::Player);
    assert!(
        (app.playback.speed - 1.1).abs() < 1e-9,
        "speed persisted on close"
    );
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    focus_current_settings_field(&mut app, Field::Band(0));
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    focus_current_settings_field(&mut app, Field::Band(0));
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab (EQ section lives here)
    focus_current_settings_field(&mut app, Field::Band(0));
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General); row 0 = language
    let cookies_row = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::CookiesFile)
        .expect("cookies file field");
    for _ in 0..cookies_row {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (General)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing the key
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
        "committing a changed key must reload the DJ Gem actor"
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
fn editing_api_key_requires_confirmation() {
    // Activating the masked key row clears the buffer, so a stray Enter/click could blank the
    // saved key. Guard it: the first activation asks first; only a confirm enters edit mode.
    let mut app = app_playing(1, 0);
    app.config.gemini_api_key = Some("KEEPME".to_owned());
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row

    // Activation asks first — it does NOT drop straight into edit mode.
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(
        app.overlays.pending_settings_confirm,
        Some(crate::settings::SettingsConfirm::EditApiKey)
    );
    assert!(!app.settings.as_ref().unwrap().editing_text);

    // Cancelling dismisses the popup and leaves the key untouched (never entered the editor).
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!(!app.settings.as_ref().unwrap().editing_text);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.gemini_api_key,
        "KEEPME"
    );

    // Ask again and confirm (Enter): now edit mode begins with a freshly cleared buffer.
    app.update(Msg::Key(key(KeyCode::Enter))); // request
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm
    assert!(app.overlays.pending_settings_confirm.is_none());
    assert!(app.settings.as_ref().unwrap().editing_text);
    assert_eq!(app.settings.as_ref().unwrap().draft.gemini_api_key, "");
}

#[test]
fn api_key_persists_when_leaving_settings_via_close() {
    // The reported bug: type a key, then leave with Esc/q (the intuitive move) — the
    // key must survive.
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // Model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> buffer cleared
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> masked buffer cleared
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
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft seeds from config)
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Tab))); // → DJ Gem tab (index 4)
    }
    app.update(Msg::Key(key(KeyCode::Down))); // AiEnabled -> Model
    app.update(Msg::Key(key(KeyCode::Down))); // → API key row
    app.update(Msg::Key(key(KeyCode::Enter))); // request edit → confirm popup
    app.update(Msg::Key(key(KeyCode::Enter))); // confirm → start editing -> buffer cleared, key stashed
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
    assert_eq!(
        save_config(&cmds).unwrap().gemini_api_key.as_deref(),
        Some("KEEPME")
    );
}

#[test]
fn reset_all_re_enables_ai() {
    // Reset All must restore *every* field to its default, including the DJ Gem on/off switch —
    // otherwise a user who disabled DJ Gem then reset would be stranded with DJ Gem off.
    let mut app = app_playing(1, 0);
    app.config.ai_enabled = Some(false);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open (draft.ai_enabled seeds false)
    assert!(!app.settings.as_ref().unwrap().draft.ai_enabled);
    app.settings_reset_all();
    assert!(
        app.settings.as_ref().unwrap().draft.ai_enabled,
        "reset returns DJ Gem to its default (enabled)"
    );
}
