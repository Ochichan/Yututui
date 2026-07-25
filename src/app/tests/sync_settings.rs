use super::*;

#[test]
fn keyboard_moves_and_activates_the_selected_sync_row() {
    let mut app = App::new(100);
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = SyncArea::PersonalState;

    assert_eq!(app.personal_state.sync_ui.row, 0);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.personal_state.sync_ui.row, 1);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        app.personal_state.sync_ui.modal_open(),
        "Enter should activate the selected Join row"
    );
}

#[test]
fn clicking_the_sync_tab_requests_a_fresh_status_projection() {
    let mut app = App::new(100);
    app.open_settings();

    let cmds = click_target(
        &mut app,
        MouseTarget::SettingsTab(SettingsTab::Sync.index()),
    );
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Sync);
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh { .. }))
    )));
}

#[test]
fn sync_rows_have_dedicated_mouse_targets_and_activate_on_click() {
    let mut app = App::new(100);
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = SyncArea::PersonalState;

    let _ = render_app_buffer(&app, 80, 24);
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|region| region.target == MouseTarget::SettingsSyncRow(1))
    );

    let cmds = click_target(&mut app, MouseTarget::SettingsSyncRow(1));
    assert!(cmds.is_empty());
    assert_eq!(app.personal_state.sync_ui.row, 1);
    assert!(
        app.personal_state.sync_ui.modal_open(),
        "click should activate the selected Join row"
    );
}

#[test]
fn thirty_column_sync_layout_keeps_a_selectable_row_visible() {
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = SyncArea::PersonalState;

    let buffer = render_app_buffer(&app, 30, 30);
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|region| region.target == MouseTarget::SettingsSyncRow(0)),
        "the compact Settings layout should retain a keyboard-and-mouse selectable Sync row"
    );
    assert!(buffer_contains(&buffer, "Create"));
}

#[test]
fn renderer_uses_the_live_sync_projection() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = SyncArea::PersonalState;
    app.personal_state.sync_ui.lifecycle = crate::sync::service::SyncLifecycleState::Active;
    app.personal_state.sync_ui.status.configured = true;
    app.personal_state.sync_ui.status.state = crate::sync::SyncHealthState::UpToDate;

    let buffer = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buffer, "Sync now"));
    assert!(!buffer_contains(&buffer, "Create encrypted sync"));
}

#[test]
fn renderer_exposes_each_recoverable_unfinished_connection_action() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = needs_cleanup_app();
    app.config.retro_mode = true;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = SyncArea::PersonalState;

    let buffer = render_app_buffer(&app, 100, 30);

    assert!(buffer_contains(&buffer, "Continue saved connection"));
    assert!(buffer_contains(&buffer, "Enter connection details again"));
    assert!(buffer_contains(&buffer, "Discard unfinished connection"));
}

#[test]
fn printable_quit_binding_is_text_inside_sync_connection_form() {
    let mut app = App::new(100);
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert("global.quit".to_owned(), "q".to_owned());
    app.keymap = crate::keymap::KeyMap::from_overrides(&bindings);
    assert!(matches!(
        app.keymap
            .global_action(crate::keymap::Chord::from(key(KeyCode::Char('q')))),
        Some(crate::keymap::Action::Quit)
    ));
    app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
        form: SyncConnectionForm::join(),
        confirm: false,
    });

    let commands = app.update(Msg::Key(key(KeyCode::Char('q'))));

    assert!(commands.is_empty());
    assert!(!app.should_quit);
    let Some(SyncWizard::Join { form, .. }) = app.personal_state.sync_ui.wizard.as_ref() else {
        panic!("join form should remain open");
    };
    assert_eq!(
        form.display_value(SyncConnectionField::Endpoint),
        "q",
        "the printable global mapping belongs to the focused text field"
    );
}

#[test]
fn backtab_moves_to_the_previous_sync_connection_field() {
    let mut app = App::new(100);
    let mut form = SyncConnectionForm::join();
    form.select_field(true, 1);
    app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
        form,
        confirm: false,
    });

    app.update(Msg::Key(key(KeyCode::BackTab)));

    let Some(SyncWizard::Join { form, .. }) = app.personal_state.sync_ui.wizard.as_ref() else {
        panic!("join form should remain open");
    };
    assert!(matches!(
        form.current_field(true),
        SyncConnectionField::Endpoint
    ));
}

#[test]
fn revealed_sync_secret_updates_the_keyboard_hint() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    let mut form = SyncConnectionForm::join();
    form.select_field(true, 2);
    app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
        form,
        confirm: false,
    });
    for ch in "z9Q".chars() {
        app.update(Msg::Key(key(KeyCode::Char(ch))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('r'))));

    let buffer = render_app_buffer(&app, 80, 24);

    assert!(buffer_contains(&buffer, "z9Q"));
    assert!(buffer_contains(&buffer, "Ctrl+R hide"));
}

#[test]
fn paired_double_click_cannot_advance_a_sync_wizard_twice() {
    let mut app = App::new(100);
    app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
        form: SyncConnectionForm::join(),
        confirm: false,
    });

    let commands = app.update(Msg::MouseDoubleClick { col: 10, row: 10 });

    assert!(commands.is_empty());
    assert!(matches!(
        app.personal_state.sync_ui.wizard,
        Some(SyncWizard::Join { confirm: false, .. })
    ));
}

fn sync_overview(state: crate::sync::SyncHealthState) -> crate::sync::service::SyncOverview {
    crate::sync::service::SyncOverview {
        status: crate::sync::service::SyncStatusReport {
            state,
            label: state.label().to_owned(),
            ..Default::default()
        },
        lifecycle: crate::sync::service::SyncLifecycleState::Absent,
        devices: Vec::new(),
        audit: Vec::new(),
    }
}

#[test]
fn refresh_is_single_flight_and_only_the_latest_request_applies() {
    let mut app = App::new(100);
    let first = app.request_sync_ui_refresh();
    let [
        Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh {
            flow_id,
            request_id: first_request,
            ..
        })),
    ] = first.as_slice()
    else {
        panic!("first refresh should start");
    };
    app.queue_sync_ui_refresh();
    let latest_request = app.personal_state.sync_ui.refresh_request_id;
    assert_ne!(*first_request, latest_request);

    let follow_up = app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::Refreshed {
        flow_id: *flow_id,
        request_id: *first_request,
        result: Box::new(Ok(sync_overview(
            crate::sync::SyncHealthState::NeedsAttention,
        ))),
    })));

    assert_eq!(
        app.personal_state.sync_ui.status.state,
        crate::sync::SyncHealthState::Off
    );
    assert!(matches!(
        follow_up.as_slice(),
        [Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh {
            request_id,
            ..
        }))] if *request_id == latest_request
    ));

    let duplicate = app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::Refreshed {
        flow_id: *flow_id,
        request_id: *first_request,
        result: Box::new(Ok(sync_overview(
            crate::sync::SyncHealthState::NeedsAttention,
        ))),
    })));
    assert!(duplicate.is_empty());
    assert!(matches!(
        app.personal_state.sync_ui.busy,
        Some(crate::app::sync_ui::SyncBusy::Refresh)
    ));

    app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::Refreshed {
        flow_id: *flow_id,
        request_id: latest_request,
        result: Box::new(Ok(sync_overview(crate::sync::SyncHealthState::UpToDate))),
    })));
    assert_eq!(
        app.personal_state.sync_ui.status.state,
        crate::sync::SyncHealthState::UpToDate
    );
    assert!(app.personal_state.sync_ui.busy.is_none());
}

#[test]
fn terminal_sync_worker_error_queues_a_fresh_overview() {
    let mut app = App::new(100);
    app.personal_state.sync_ui.flow_id = 7;
    app.personal_state.sync_ui.busy = Some(crate::app::sync_ui::SyncBusy::RecoveryExport);

    let commands = app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::RecoveryExported {
        flow_id: 7,
        result: Box::new(Err(crate::sync::service::SyncServiceError::Storage)),
    })));

    assert!(matches!(
        commands.as_slice(),
        [Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh { .. }))]
    ));
    assert!(matches!(
        app.personal_state.sync_ui.wizard,
        Some(SyncWizard::Result { success: false, .. })
    ));
}

#[test]
fn sync_rows_are_inert_while_status_reports_syncing() {
    let mut app = App::new(100);
    app.personal_state.sync_ui.lifecycle = crate::sync::service::SyncLifecycleState::Active;
    app.personal_state.sync_ui.status.configured = true;
    app.personal_state.sync_ui.status.state = crate::sync::SyncHealthState::Syncing;
    app.personal_state.sync_ui.row = 0;

    assert!(app.activate_sync_row().is_empty());
    assert!(app.personal_state.sync_ui.wizard.is_none());
}

fn needs_cleanup_app() -> App {
    let mut app = App::new(100);
    app.personal_state.sync_ui.lifecycle = crate::sync::service::SyncLifecycleState::NeedsCleanup;
    app.personal_state.sync_ui.status.state = crate::sync::SyncHealthState::NeedsAttention;
    app.personal_state.sync_ui.status.failure = Some(crate::sync::SyncFailureKind::Storage);
    app
}

#[test]
fn needs_cleanup_offers_resume_reentry_discard_and_activity() {
    let app = needs_cleanup_app();
    let model = app.sync_settings_model();

    assert_eq!(model.failure, None);
    assert_eq!(
        model.rows,
        vec![
            crate::settings::sync::SyncRow::Notice(crate::settings::sync::SyncNotice::NeedsCleanup),
            crate::settings::sync::SyncRow::Action(
                crate::settings::sync::SyncRowAction::ResumeConnection
            ),
            crate::settings::sync::SyncRow::Action(
                crate::settings::sync::SyncRowAction::ReenterConnection
            ),
            crate::settings::sync::SyncRow::Action(
                crate::settings::sync::SyncRowAction::DiscardConnection
            ),
            crate::settings::sync::SyncRow::Action(crate::settings::sync::SyncRowAction::Activity),
        ]
    );
    assert_eq!(app.personal_state.sync_ui.row_count(), 5);
}

#[test]
fn needs_cleanup_resume_is_a_move_only_worker_request() {
    let mut app = needs_cleanup_app();
    app.personal_state.sync_ui.row = 1;

    let commands = app.activate_sync_row();

    assert!(matches!(
        commands.as_slice(),
        [Cmd::Data(DataCmd::SyncUi(SyncUiCommand::JoinResume {
            flow_id,
            ..
        }))] if *flow_id == app.personal_state.sync_ui.flow_id
    ));
    assert!(matches!(
        app.personal_state.sync_ui.busy,
        Some(crate::app::sync_ui::SyncBusy::PairJoinPoll)
    ));
}

#[test]
fn needs_cleanup_can_reenter_the_same_connection_details() {
    let mut app = needs_cleanup_app();
    app.personal_state.sync_ui.row = 2;

    assert!(app.activate_sync_row().is_empty());

    assert!(matches!(
        app.personal_state.sync_ui.wizard,
        Some(SyncWizard::Join { confirm: false, .. })
    ));
}

#[test]
fn unfinished_connection_discard_requires_a_separate_confirmation() {
    let mut app = needs_cleanup_app();
    app.personal_state.sync_ui.row = 3;

    assert!(app.activate_sync_row().is_empty());
    assert!(matches!(
        app.personal_state.sync_ui.wizard,
        Some(SyncWizard::DiscardJoinConfirm)
    ));

    let commands = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        commands.as_slice(),
        [Cmd::Data(DataCmd::SyncUi(SyncUiCommand::DiscardJoin {
            flow_id
        }))] if *flow_id == app.personal_state.sync_ui.flow_id
    ));
    assert!(matches!(
        app.personal_state.sync_ui.busy,
        Some(crate::app::sync_ui::SyncBusy::PairJoinDiscard)
    ));
}

#[test]
fn cancelling_discard_confirmation_preserves_the_unfinished_connection() {
    let mut app = needs_cleanup_app();
    app.personal_state.sync_ui.wizard = Some(SyncWizard::DiscardJoinConfirm);

    assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    assert!(app.personal_state.sync_ui.wizard.is_none());
    assert!(app.personal_state.sync_ui.busy.is_none());
}

#[test]
fn discarded_connection_reports_local_data_was_preserved_and_refreshes() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = needs_cleanup_app();
    app.personal_state.sync_ui.flow_id = 9;
    app.personal_state.sync_ui.busy = Some(crate::app::sync_ui::SyncBusy::PairJoinDiscard);

    let commands = app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::JoinDiscarded {
        flow_id: 9,
        result: Ok(()),
    })));

    assert!(matches!(
        commands.as_slice(),
        [Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh { .. }))]
    ));
    assert!(matches!(
        app.personal_state.sync_ui.wizard,
        Some(SyncWizard::Result {
            success: true,
            ref message,
        }) if message.contains("Local listening data was not changed")
    ));
}
