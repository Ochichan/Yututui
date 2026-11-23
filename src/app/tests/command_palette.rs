//! Command Palette (`Ctrl+K`) tests: chapter jump, Settings jump, in-memory theme preview &
//! Esc revert, Enter-only EQ apply, context-action gating, Korean query, and footer discoverability.

use super::*;
use crate::eq::EqPreset;
use crate::player::{Chapter, SeekPrecision};
use crate::settings::{Field, SettingsTab};
use crate::theme::ThemePreset;

fn ctrl_k() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
}

fn type_str(app: &mut App, text: &str) -> Vec<Cmd> {
    let mut out = Vec::new();
    for c in text.chars() {
        out.extend(app.update(Msg::Key(key(KeyCode::Char(c)))));
    }
    out
}

#[test]
fn footer_shows_command_palette_hint_and_ctrl_k_opens_and_esc_closes() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    let buf = render_app_buffer(&app, 100, 24);
    assert!(
        (0..24).any(|y| buffer_row(&buf, y).contains("^k commands")),
        "footer should surface ^k commands on wide screens"
    );
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|r| r.target == MouseTarget::Global(Action::OpenCommandPalette)),
        "footer Ctrl+K commands hint must be clickable"
    );

    let cmds = app.update(Msg::Key(ctrl_k()));
    assert!(cmds.is_empty());
    assert!(app.overlays.command_palette.is_some());

    let modal_buf = render_app_buffer(&app, 100, 24);
    assert!(
        (0..24).any(|y| buffer_row(&modal_buf, y).contains("Command Palette")),
        "command palette modal should render title"
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(cmds.is_empty());
    assert!(app.overlays.command_palette.is_none());
}

#[test]
fn chapter_jump_via_palette_seeks_and_bumps_position_epoch() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.queue
        .set(vec![fsong("live-1", "Live Concert", "Band")], 0);
    app.install_seek_parity_state("live-1", 10.0, 3600.0);
    app.update(Msg::Player(PlayerMsg::Chapters(vec![
        Chapter {
            title: "Opening Overture".to_owned(),
            start_secs: 0.0,
        },
        Chapter {
            title: "Acoustic Encore".to_owned(),
            start_secs: 1845.0,
        },
    ])));

    let epoch_before = app.playback.position_epoch;
    app.update(Msg::Key(ctrl_k()));
    // Empty query puts current-track chapters first.
    let top_category = app
        .overlays
        .command_palette
        .as_ref()
        .and_then(|s| s.selected_entry())
        .map(|e| e.category);
    assert_eq!(top_category, Some(PaletteCategory::Chapter));

    type_str(&mut app, "encore");
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.overlays.command_palette.is_none());
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|cmd| {
        matches!(
            cmd,
            PlayerCmd::SeekAbsolute {
                seconds,
                precision: SeekPrecision::InteractiveFast,
            } if (*seconds - 1845.0).abs() < 1e-6
        )
    }));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.playback.position_epoch, epoch_before + 1);
    assert_eq!(app.playback.time_pos, Some(1845.0));
}

#[test]
fn theme_previews_in_memory_without_disk_writes_and_esc_reverts() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    let original_preset = app.theme.preset_enum();
    let original_cfg_preset = app.config.theme.preset_enum();

    app.update(Msg::Key(ctrl_k()));
    let cmds = type_str(&mut app, "theme dracula");
    assert!(cmds.is_empty(), "live preview must emit zero commands");
    assert_eq!(app.theme.preset_enum(), ThemePreset::Dracula);
    assert_eq!(
        app.config.theme.preset_enum(),
        original_cfg_preset,
        "preview must not mutate persisted config"
    );
    assert!(
        app.overlays
            .command_palette
            .as_ref()
            .is_some_and(|s| s.preview_active())
    );

    // Esc restores exact original theme with zero disk writes.
    let esc_cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(esc_cmds.is_empty());
    assert_eq!(app.theme.preset_enum(), original_preset);

    // Re-open, pick Dracula, and press Enter -> commits + emits PersistCmd::Config.
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "theme dracula");
    let commit_cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.theme.preset_enum(), ThemePreset::Dracula);
    assert_eq!(app.config.theme.preset_enum(), ThemePreset::Dracula);
    assert!(
        commit_cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_)))),
        "Enter on a theme must persist config"
    );
}

#[test]
fn eq_preset_never_live_previews_and_applies_only_on_enter() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    let initial_eq = app.audio.preset;
    app.update(Msg::Key(ctrl_k()));
    let cmds = type_str(&mut app, "eq rock");
    assert!(cmds.is_empty(), "EQ highlight must never send player cmds");
    assert_eq!(
        app.audio.preset, initial_eq,
        "EQ must not preview on select"
    );
    assert!(
        !app.overlays
            .command_palette
            .as_ref()
            .is_some_and(|s| s.preview_active())
    );

    let mut enter_cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        enter_cmds
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|cmd| matches!(cmd, PlayerCmd::SetAudioFilter(_))),
        "Enter on EQ preset must emit SetAudioFilter"
    );
    admit_player_transition(&mut app, &mut enter_cmds);
    assert_eq!(app.audio.preset, EqPreset::Rock);
}

#[test]
fn settings_jump_focuses_exact_tab_and_field_row() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "setting skip non-music");
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert_eq!(app.mode, Mode::Settings);
    let st = app.settings.as_deref().expect("settings opened");
    assert_eq!(st.tab, SettingsTab::Playback);
    assert_eq!(st.fields().get(st.row), Some(&Field::SkipNonMusic));
}

#[test]
fn disabled_context_action_refuses_execution_with_reason_and_korean_query_works() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::Korean);

    let mut app = App::new(100);
    // Nothing loaded -> Next chapter is disabled.
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "다음 챕터");
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    let state = app
        .overlays
        .command_palette
        .as_ref()
        .expect("palette stays open on disabled entry");
    assert!(state.error_hint.is_some());

    // Render across standard and narrow terminals without panic.
    let _ = render_app_buffer(&app, 80, 24);
    let _ = render_app_buffer(&app, 30, 16);
}
