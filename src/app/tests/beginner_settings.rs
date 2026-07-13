use super::*;

fn close_settings_and_admit(app: &mut App) -> Vec<Cmd> {
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    admit_player_transition(app, &mut cmds);
    cmds
}

#[test]
fn beginner_mode_toggle_previews_live_and_reenable_requests_restart() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = false;
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::BeginnerMode)
    );
    assert!(!app.beginner_mode());

    app.update(Msg::Key(key(KeyCode::Right)));
    assert!(
        app.beginner_mode(),
        "the Settings draft previews immediately"
    );
    assert!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .restart_beginner_tutorial
    );
}

#[test]
fn saving_while_beginner_mode_is_already_off_preserves_future_progress() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = false;
    app.config.beginner_tutorial = crate::config::BeginnerTutorialProgress {
        content_version: crate::config::BEGINNER_TUTORIAL_VERSION + 1,
        next_step: "future_spatial_mixer".to_owned(),
    };

    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let cmds = close_settings_and_admit(&mut app);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert!(!saved.beginner_mode);
    assert_eq!(saved.beginner_tutorial, app.config.beginner_tutorial);
    assert_eq!(saved.beginner_tutorial.next_step, "future_spatial_mixer");
}

#[test]
fn disabling_suppressed_future_beginner_mode_preserves_its_opaque_cursor() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = true;
    app.config.beginner_tutorial = crate::config::BeginnerTutorialProgress {
        content_version: crate::config::BEGINNER_TUTORIAL_VERSION + 1,
        next_step: "future_spatial_mixer".to_owned(),
    };
    app.prepare_beginner_onboarding(true);
    assert!(!app.onboarding.active());

    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.update(Msg::Key(key(KeyCode::Right)));
    let cmds = close_settings_and_admit(&mut app);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert!(!saved.beginner_mode);
    assert_eq!(
        saved.beginner_tutorial.content_version,
        crate::config::BEGINNER_TUTORIAL_VERSION + 1
    );
    assert_eq!(saved.beginner_tutorial.next_step, "future_spatial_mixer");
}

#[test]
fn saving_reenabled_beginner_mode_schedules_welcome_for_next_launch_only() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = false;
    app.config.beginner_tutorial.next_step = "library".to_owned();
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.update(Msg::Key(key(KeyCode::Right)));

    let cmds = close_settings_and_admit(&mut app);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert!(saved.beginner_mode);
    assert_eq!(
        saved.beginner_tutorial,
        crate::config::BeginnerTutorialProgress::welcome()
    );
    assert!(
        !app.onboarding.active(),
        "re-enabling must not launch the tour in the current session"
    );
}

#[test]
fn active_tour_finishes_only_after_beginner_mode_save_is_admitted() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = true;
    app.config.beginner_tutorial.next_step = "finish".to_owned();
    app.prepare_beginner_onboarding(true);
    assert!(app.onboarding.active());

    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
    app.update(Msg::Key(key(KeyCode::Right)));
    assert!(!app.settings.as_ref().unwrap().draft.beginner_mode);

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(
        app.config.beginner_mode,
        "the rejected-or-pending save must leave persisted state untouched"
    );
    assert!(app.onboarding.active());
    assert!(save_config(&cmds).is_none());

    admit_player_transition(&mut app, &mut cmds);
    assert!(!app.config.beginner_mode);
    assert_eq!(
        app.config.beginner_tutorial,
        crate::config::BeginnerTutorialProgress::welcome()
    );
    assert!(!app.onboarding.active());
    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
            .count(),
        1,
        "an admitted completion persists one final Config snapshot"
    );
}

#[test]
fn rejected_beginner_completion_keeps_the_tour_and_settings_draft_open() {
    let mut app = app_playing(1, 0);
    app.config.beginner_mode = true;
    app.config.beginner_tutorial.next_step = "finish".to_owned();
    app.prepare_beginner_onboarding(true);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
    app.update(Msg::Key(key(KeyCode::Right)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(
        reject_player_transition(&mut app, cmds, crate::util::delivery::DeliveryError::Busy,)
            .is_empty()
    );
    assert!(app.config.beginner_mode);
    assert_eq!(app.config.beginner_tutorial.next_step, "finish");
    assert!(app.onboarding.active());
    assert!(
        app.settings
            .as_ref()
            .is_some_and(|settings| { !settings.draft.beginner_mode })
    );
}
