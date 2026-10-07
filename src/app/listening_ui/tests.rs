use super::*;
use crate::listening::DjPresetId;
use ratatui::{Terminal, backend::TestBackend};

fn app() -> App {
    let mut app = App::new(50);
    app.config.listening_records_enabled = Some(true);
    app.config.listening_local_scope = Some("listening-ui-tests".to_owned());
    let state = crate::personal_state::legacy_state(
        &app.library,
        &app.playlists,
        &app.signals,
        &app.station,
    )
    .unwrap();
    app.install_personal_state_runtime(state).unwrap();
    app
}

fn preset(name: &str) -> DjPreset {
    DjPreset {
        preset_id: DjPresetId::new("test-preset").unwrap(),
        name: name.to_owned(),
        snapshot: crate::streaming::SessionTaste::default().snapshot(),
    }
}

fn saved_app() -> App {
    let mut app = app();
    let commands = app.commit_listening_change(ListeningOperation::UpsertDjPreset {
        preset: preset("Quiet work"),
    });
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Cmd::Persist(_)))
    );
    app.open_listening(ListeningTab::Presets);
    app
}

#[test]
fn first_open_requires_activation_without_upgrading_the_ledger() {
    let mut app = app();
    app.config.listening_records_enabled = Some(false);
    let schema = app.personal_state.ledger.schema_version;
    assert!(app.open_listening(ListeningTab::Passport).is_empty());
    assert!(!app.listening_records_enabled());
    assert!(matches!(
        app.overlays.listening.as_ref().unwrap().editing,
        Some(ListeningEdit::Enable)
    ));
    let commands = app.listening_control(ListeningControl::Confirm);
    assert!(app.listening_records_enabled());
    assert_eq!(app.personal_state.ledger.schema_version, schema);
    assert!(
        commands
            .iter()
            .all(|command| matches!(command, Cmd::Persist(PersistCmd::Config(_))))
    );
    assert!(app.overlays.listening.as_ref().unwrap().editing.is_none());
}

#[test]
fn preset_editor_saves_a_snapshot_without_touching_playback() {
    let mut app = app();
    app.queue.set(
        vec![Song::remote("dQw4w9WgXcQ", "Track", "Artist", "4:00")],
        0,
    );
    let before = app.queue.snapshot();
    app.open_listening(ListeningTab::Presets);
    app.listening_control(ListeningControl::New);
    app.overlays.listening.as_mut().unwrap().input = "집중 시간".to_owned();
    let commands = app.listening_control(ListeningControl::Confirm);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Cmd::Persist(_)))
    );
    let records = ListeningProjection::from_ledger(&app.personal_state.ledger).unwrap();
    assert_eq!(
        records.dj_presets.values().next().unwrap()[0].name,
        "집중 시간"
    );
    assert_eq!(
        serde_json::to_value(app.queue.snapshot()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, Cmd::PlayerControl(_)))
    );
}

#[test]
fn incoming_change_rejects_a_stale_editor_and_mouse_action() {
    let mut app = saved_app();
    let old_revision = app.personal_state.ledger.revision;
    app.listening_control(ListeningControl::Edit);
    app.overlays.listening.as_mut().unwrap().input = "Stale draft".to_owned();
    app.commit_listening_change(ListeningOperation::UpsertDjPreset {
        preset: preset("Changed elsewhere"),
    });
    assert!(app.listening_control(ListeningControl::Confirm).is_empty());
    assert!(app.overlays.listening.as_ref().unwrap().error.is_some());
    assert!(
        app.listening_mouse(ListeningAction {
            revision: old_revision,
            control: ListeningControl::Delete
        })
        .is_empty()
    );
    assert_eq!(
        ListeningProjection::from_ledger(&app.personal_state.ledger)
            .unwrap()
            .dj_presets
            .values()
            .next()
            .unwrap()[0]
            .name,
        "Changed elsewhere"
    );
}

#[test]
fn escape_cancels_the_editor_before_closing_and_text_does_not_trigger_actions() {
    let mut app = saved_app();
    app.listening_control(ListeningControl::Edit);
    let key = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(app.listening_key(key, Chord::from(key)).is_empty());
    assert!(app.overlays.listening.as_ref().unwrap().input.contains(' '));
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.listening_key(esc, Chord::from(esc));
    assert!(app.overlays.listening.as_ref().unwrap().editing.is_none());
    app.listening_key(esc, Chord::from(esc));
    assert!(app.overlays.listening.is_none());
}

#[test]
fn narrow_and_full_dialogs_render_localized_controls_within_the_frame() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = saved_app();
    for language in crate::i18n::Language::CYCLE {
        crate::i18n::set_language(language);
        for (width, height) in [(30, 12), (80, 24), (120, 40)] {
            for tab in ListeningTab::ALL {
                app.open_listening(tab);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| crate::ui::views::listening::render(frame, &app, frame.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let text = buffer
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                let compact = text
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>();
                let label = tab
                    .label()
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>();
                assert!(
                    compact.contains(&label),
                    "missing {tab:?} at {width}x{height}: {text}"
                );
                assert!(
                    text.contains("Esc"),
                    "close control must remain visible: {text}"
                );
            }
        }
    }
}

#[test]
fn narrow_activation_keeps_the_compatibility_notice_and_confirmation_visible() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app();
    app.config.listening_records_enabled = Some(false);
    for language in crate::i18n::Language::CYCLE {
        crate::i18n::set_language(language);
        app.open_listening(ListeningTab::Bookmarks);
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        terminal
            .draw(|frame| crate::ui::views::listening::render(frame, &app, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = text
            .chars()
            .filter(|character| !character.is_whitespace() && *character != '│')
            .collect::<String>();
        let notice = t!(
            "older versions will stop syncing",
            "구버전은 동기화를 멈춥니다",
            "旧版は同期を停止します"
        );
        assert!(
            compact.contains(
                &notice
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>()
            ),
            "compatibility notice clipped: {text}"
        );
        assert!(text.contains("Enter") && text.contains("Esc"));
        for row in 1..11 {
            assert_eq!(
                terminal.backend().buffer()[(29, row)].symbol(),
                "│",
                "activation text crossed its border"
            );
        }
    }
}

#[test]
fn same_name_preset_details_expose_the_saved_preferences_without_loading_them() {
    let mut app = app();
    let mut first = preset("Focus");
    first.snapshot.seeds.push(crate::streaming::taste::Seed {
        term: crate::streaming::taste::SeedTerm::new("jazz").unwrap(),
        polarity: crate::streaming::SeedPolarity::MoreLike,
    });
    let mut second = first.clone();
    second.snapshot.seeds[0].term = crate::streaming::taste::SeedTerm::new("piano").unwrap();
    let first = ListeningRow::Preset(first);
    let second = ListeningRow::Preset(second);
    assert_ne!(first.full_detail(&app), second.full_detail(&app));
    assert!(first.full_detail(&app).contains("+ jazz"));
    assert!(second.full_detail(&app).contains("+ piano"));
    let before = app.streaming.taste.snapshot();
    app.overlays.listening = Some(ListeningDialog {
        rows: vec![first, second],
        ..Default::default()
    });
    assert!(app.listening_control(ListeningControl::Details).is_empty());
    assert!(matches!(
        app.overlays.listening.as_ref().unwrap().editing,
        Some(ListeningEdit::Inspect(_))
    ));
    assert_eq!(before, app.streaming.taste.snapshot());
}

#[test]
fn narrow_errors_keep_recovery_instructions_visible_and_return_to_the_list() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = saved_app();
    app.listening_error(
        "Records changed while editing. Press Esc and open the record again.".to_owned(),
    );
    let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
    terminal
        .draw(|frame| crate::ui::views::listening::render(frame, &app, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let compact = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '│')
        .collect::<String>();
    assert!(compact.contains("PressEscandopentherecordagain."), "{text}");
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.listening_key(esc, Chord::from(esc));
    assert!(app.overlays.listening.as_ref().unwrap().error.is_none());
}
