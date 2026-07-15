use super::*;

fn theme_bytes(theme: &ThemeConfig) -> Vec<u8> {
    serde_json::to_vec(theme).expect("theme serializes")
}

fn config_bytes(config: &Config) -> Vec<u8> {
    serde_json::to_vec(config).expect("config serializes")
}

#[test]
fn local_deck_defaults_to_local_launch_and_remembers_each_mode_theme() {
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.theme
        .set_override(crate::theme::ThemeRole::Accent, "#123456")
        .unwrap();
    app.config.theme = app.theme.clone();
    let normal = app.theme.clone();

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert!(app.local_dedicated_mode);
    assert_eq!(
        app.theme.preset_enum(),
        crate::theme::ThemePreset::LocalLaunch
    );
    assert_eq!(
        app.local_mode.normal_mode_theme.as_ref().map(theme_bytes),
        Some(theme_bytes(&normal))
    );

    app.theme.set_preset(crate::theme::ThemePreset::RosePine);
    app.theme
        .set_override(crate::theme::ThemeRole::BorderFocused, "#ABCDEF")
        .unwrap();
    let local = app.theme.clone();

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert!(!app.local_dedicated_mode);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&normal));

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&local));
}

#[test]
fn rejected_local_switch_preserves_theme_config_and_stashes_until_retry() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut config = Config::default();
        config.theme.set_preset(crate::theme::ThemePreset::Midnight);
        let mut local = ThemeConfig::local_launch();
        local.set_preset(crate::theme::ThemePreset::RosePine);
        local
            .set_override(crate::theme::ThemeRole::Accent, "#ABCDEF")
            .unwrap();
        config.local_theme = Some(local.clone());

        let mut app = app_playing(1, 0);
        app.apply_config(&config);
        app.local_mode.pending_confirm = Some(LocalModeConfirm::Enter);
        let before_theme = theme_bytes(&app.theme);
        let before_config = config_bytes(&app.config);
        let before_local_stash = app.local_mode.local_mode_theme.as_ref().map(theme_bytes);
        let before_normal_stash = app.local_mode.normal_mode_theme.as_ref().map(theme_bytes);

        let cmds = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
        assert_eq!(theme_bytes(&app.theme), before_theme);
        assert!(reject_player_transition(&mut app, cmds, error).is_empty());

        assert!(!app.local_dedicated_mode);
        assert_eq!(theme_bytes(&app.theme), before_theme);
        assert_eq!(config_bytes(&app.config), before_config);
        assert_eq!(
            app.local_mode.local_mode_theme.as_ref().map(theme_bytes),
            before_local_stash
        );
        assert_eq!(
            app.local_mode.normal_mode_theme.as_ref().map(theme_bytes),
            before_normal_stash
        );
        assert_eq!(
            app.local_mode.pending_confirm,
            Some(LocalModeConfirm::Enter)
        );

        let mut retry = app.apply_local_mode_confirm(LocalModeConfirm::Enter);
        admit_player_transition(&mut app, &mut retry);
        assert!(app.local_dedicated_mode);
        assert_eq!(theme_bytes(&app.theme), theme_bytes(&local));
    }
}

#[test]
fn local_settings_save_persists_only_the_local_theme_slot() {
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.theme
        .set_override(crate::theme::ThemeRole::Accent, "#123456")
        .unwrap();
    app.config.theme = app.theme.clone();
    let normal = app.theme.clone();

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.open_settings();
    {
        let draft = &mut app.settings.as_mut().unwrap().draft.theme;
        draft.set_preset(crate::theme::ThemePreset::Custom);
        draft
            .set_override(crate::theme::ThemeRole::Accent, "#ABCDEF")
            .unwrap();
        draft
            .set_override(crate::theme::ThemeRole::BorderFocused, "#FEDCBA")
            .unwrap();
    }
    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);

    let saved = save_config(&cmds).expect("Local Settings save persists config");
    assert_eq!(theme_bytes(&saved.theme), theme_bytes(&normal));
    let saved_local = saved.local_theme.as_ref().expect("Local theme slot");
    assert_eq!(saved_local.preset_enum(), crate::theme::ThemePreset::Custom);
    assert_eq!(
        saved_local.effective_hex(crate::theme::ThemeRole::Accent),
        "#ABCDEF"
    );
    assert_eq!(
        saved_local.effective_hex(crate::theme::ThemeRole::BorderFocused),
        "#FEDCBA"
    );
    assert_eq!(theme_bytes(&app.theme), theme_bytes(saved_local));

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&normal));
    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(saved_local));
}

#[test]
fn local_reset_all_restores_and_persists_factory_local_launch_theme() {
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    let normal = app.theme.clone();

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.theme.set_preset(crate::theme::ThemePreset::RosePine);
    app.open_settings();
    app.settings.as_mut().unwrap().draft.theme = app.theme.clone();

    let mut reset = app.settings_reset_all();
    admit_player_transition(&mut app, &mut reset);
    assert_eq!(
        app.theme.preset_enum(),
        crate::theme::ThemePreset::LocalLaunch
    );
    assert_eq!(
        app.settings.as_ref().unwrap().draft.theme.preset_enum(),
        crate::theme::ThemePreset::LocalLaunch
    );

    let mut save = app.close_settings();
    admit_player_transition(&mut app, &mut save);
    let saved = save_config(&save).expect("Local factory reset persists config");
    assert_eq!(theme_bytes(&saved.theme), theme_bytes(&normal));
    assert_eq!(
        saved.local_theme.as_ref().map(ThemeConfig::preset_enum),
        Some(crate::theme::ThemePreset::LocalLaunch)
    );
}

#[test]
fn local_retro_seed_is_saved_to_local_slot_without_overwriting_normal_theme() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    let normal = app.theme.clone();

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    app.open_settings();
    app.settings_toggle_retro_mode();
    assert_eq!(app.theme.preset_enum(), crate::theme::ThemePreset::Retro);

    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);
    let saved = save_config(&cmds).expect("Retro Local Settings save persists config");
    assert_eq!(theme_bytes(&saved.theme), theme_bytes(&normal));
    assert_eq!(
        saved.local_theme.as_ref().map(ThemeConfig::preset_enum),
        Some(crate::theme::ThemePreset::Retro)
    );

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&normal));
    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Enter);
    assert_eq!(app.theme.preset_enum(), crate::theme::ThemePreset::Retro);
}

#[test]
fn persisted_local_theme_survives_restart_and_last_local_session_restore() {
    let mut config = Config::default();
    config.theme.set_preset(crate::theme::ThemePreset::Midnight);
    config
        .theme
        .set_override(crate::theme::ThemeRole::Accent, "#123456")
        .unwrap();
    let normal = config.theme.clone();
    let mut local = ThemeConfig::local_launch();
    local.set_preset(crate::theme::ThemePreset::Custom);
    local
        .set_override(crate::theme::ThemeRole::Accent, "#ABCDEF")
        .unwrap();
    config.local_theme = Some(local.clone());

    let mut app = App::new(100);
    app.apply_config(&config);
    let cache = crate::session::SessionCache::from_last_mode(crate::session::LastMode::Local);
    app.restore_last_session_from_cache(&cache);

    assert!(app.local_dedicated_mode);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&local));
    assert_eq!(
        app.local_mode.normal_mode_theme.as_ref().map(theme_bytes),
        Some(theme_bytes(&normal))
    );

    super::local::admit_local_mode_confirm(&mut app, LocalModeConfirm::Exit);
    assert!(!app.local_dedicated_mode);
    assert_eq!(theme_bytes(&app.theme), theme_bytes(&normal));
}

#[test]
fn local_exit_clears_find_transients_only_after_admission_and_stales_workers() {
    use crate::local::find::{
        LocalFindCorpus, LocalFindCorpusOptions, LocalFindCorpusRevision, LocalFindQuery,
        LocalFindScope, LocalFindSort,
    };
    use crate::util::delivery::DeliveryError;

    let track = super::local::local_deck_track(
        "/music/Exit Find.flac",
        "Exit Find",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Pop"],
        1,
    );
    let mut app = super::local::app_with_local_deck_index(vec![track.clone()]);
    app.queue.set(vec![track.to_song()], 0);
    let local_queue = app.queue.snapshot();
    let index_revision = app.local_mode.index.revision;

    let corpus_revision = LocalFindCorpusRevision {
        index: index_revision,
        playlists: app.playlists.revision(),
        downloads: 0,
        options: crate::local::model::stable_hash_segments(&[app
            .config
            .effective_download_dir()
            .to_string_lossy()
            .as_bytes()]),
    };
    let corpus = std::sync::Arc::new(LocalFindCorpus::build(
        app.local_mode.index.index.tracks(),
        &[],
        corpus_revision,
        &LocalFindCorpusOptions::default(),
    ));
    let query = LocalFindQuery::parse("Exit").expect("valid Local Find query");
    let snapshot = corpus.search(&query, LocalFindScope::Tracks, LocalFindSort::Title, 41);
    let stale_snapshot = snapshot.clone();
    let source = snapshot.hits().next().expect("track hit").id.clone();
    app.mode = Mode::Search;
    app.local_mode.find.query = "Exit".to_owned();
    app.local_mode.find.select_all = true;
    app.local_mode.find.focus = LocalFindFocus::Results;
    app.local_mode.find.scope = LocalFindScope::Tracks;
    app.local_mode.find.sort = LocalFindSort::Title;
    app.local_mode.find.selected = 3;
    app.local_mode.find.searching = true;
    app.local_mode.find.request_id = 41;
    app.local_mode.find.corpus_generation = 7;
    app.local_mode.find.building_revision = Some(corpus_revision);
    app.local_mode.find.corpus = Some(std::sync::Arc::clone(&corpus));
    app.local_mode.find.snapshot = Some(snapshot);
    app.local_mode.find.drill = Some(LocalFindDrill {
        title: "Exit".to_owned(),
        source,
        track_ids: vec![track.id.clone()],
        corpus_revision,
    });
    app.local_mode.find.refine_popup.open = true;
    app.local_mode.find.refine_popup.row = 2;

    app.request_local_mode_switch();
    let rejected = app.apply_local_mode_confirm(LocalModeConfirm::Exit);
    assert!(reject_player_transition(&mut app, rejected, DeliveryError::Busy).is_empty());
    assert_eq!(app.local_mode.find.query, "Exit");
    assert!(app.local_mode.find.snapshot.is_some());
    assert!(app.local_mode.find.drill.is_some());
    assert!(app.local_mode.find.refine_popup.open);
    assert_eq!(app.local_mode.find.request_id, 41);
    assert_eq!(app.local_mode.find.corpus_generation, 7);

    let mut accepted = app.apply_local_mode_confirm(LocalModeConfirm::Exit);
    admit_player_transition(&mut app, &mut accepted);
    assert!(!app.local_dedicated_mode);
    assert!(app.local_mode.find.query.is_empty());
    assert!(!app.local_mode.find.select_all);
    assert_eq!(app.local_mode.find.focus, LocalFindFocus::Input);
    assert_eq!(app.local_mode.find.scope, LocalFindScope::All);
    assert_eq!(app.local_mode.find.sort, LocalFindSort::Relevance);
    assert_eq!(app.local_mode.find.selected, 0);
    assert!(!app.local_mode.find.searching);
    assert!(app.local_mode.find.snapshot.is_none());
    assert!(app.local_mode.find.drill.is_none());
    assert!(!app.local_mode.find.refine_popup.open);
    assert!(app.local_mode.find.building_revision.is_none());
    assert_eq!(app.local_mode.find.request_id, 42);
    assert_eq!(app.local_mode.find.corpus_generation, 8);
    assert!(
        app.local_mode
            .find
            .corpus
            .as_ref()
            .is_some_and(|cached| std::sync::Arc::ptr_eq(cached, &corpus))
    );
    assert_eq!(app.local_mode.index.revision, index_revision);
    assert_eq!(app.local_mode.index.index.tracks(), &[track]);
    assert_eq!(
        serde_json::to_vec(&app.local_mode.local_mode_queue).unwrap(),
        serde_json::to_vec(&Some(local_queue)).unwrap()
    );

    let stale_corpus = std::sync::Arc::new(LocalFindCorpus::build(
        app.local_mode.index.index.tracks(),
        &[],
        corpus_revision,
        &LocalFindCorpusOptions::default(),
    ));
    assert!(
        app.update(Msg::Local(LocalMsg::FindCorpusReady {
            generation: 7,
            corpus: stale_corpus,
        }))
        .is_empty()
    );
    assert!(
        app.update(Msg::Local(LocalMsg::FindResultsReady {
            request_id: 41,
            generation: 7,
            snapshot: stale_snapshot,
        }))
        .is_empty()
    );
    assert!(app.local_mode.find.snapshot.is_none());
    assert!(
        app.local_mode
            .find
            .corpus
            .as_ref()
            .is_some_and(|cached| std::sync::Arc::ptr_eq(cached, &corpus))
    );
}
