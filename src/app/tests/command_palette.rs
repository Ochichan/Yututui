//! Command Palette (`Ctrl+K`) tests: chapter jump, Settings jump, in-memory theme preview &
//! Esc revert, Enter-only EQ apply, context-action gating, Korean query, and footer discoverability.

use super::*;
use crate::app::command_palette::PaletteTarget;
use crate::eq::EqPreset;
use crate::player::{Chapter, SeekPrecision};
use crate::settings::{Field, SettingsTab};
use crate::theme::ThemePreset;

fn select_palette(app: &mut App, mut matches_target: impl FnMut(&PaletteTarget) -> bool) {
    let visible = {
        let state = app.overlays.command_palette.as_ref().expect("palette open");
        state
            .filtered
            .iter()
            .position(|&idx| matches_target(&state.entries[idx].target))
            .expect("palette row")
    };
    let current = app.overlays.command_palette.as_ref().unwrap().selected;
    let down = visible >= current;
    for _ in 0..visible.abs_diff(current) {
        app.update(Msg::Key(key(if down {
            KeyCode::Down
        } else {
            KeyCode::Up
        })));
    }
}

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

#[test]
fn radio_palette_animation_writes_radio_master_not_music_master() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.config.animations.master = true;
    app.config.animations.radio_master = Some(false);
    app.config.animations.rain = false;
    app.radio_dedicated_mode = true;

    app.update(Msg::Key(ctrl_k()));
    let master_row = app
        .overlays
        .command_palette
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .find(|entry| matches!(entry.target, PaletteTarget::Animation(Field::AnimMaster)))
        .expect("anim master row");
    assert!(
        master_row
            .subtitle
            .as_deref()
            .is_some_and(|text| text.starts_with("off")),
        "radio master is off even while the music master is on"
    );

    type_str(&mut app, "matrix rain");
    select_palette(&mut app, |target| {
        matches!(target, PaletteTarget::Animation(Field::AnimRain))
    });
    assert!(
        app.overlays
            .command_palette
            .as_ref()
            .is_some_and(|state| state.preview_active())
    );
    assert!(
        app.config.animations.master,
        "preview must not touch music master"
    );
    assert_eq!(app.config.animations.radio_master, Some(true));
    assert!(app.config.animations.rain);
    assert!(app.animations().master);

    let esc = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(esc.is_empty());
    assert_eq!(app.config.animations.radio_master, Some(false));
    assert!(!app.config.animations.rain);
    assert!(!app.animations().master);

    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "enable animations");
    select_palette(&mut app, |target| {
        matches!(target, PaletteTarget::Animation(Field::AnimMaster))
    });
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.config.animations.master);
    assert_eq!(app.config.animations.radio_master, Some(true));
    assert!(app.animations().master);
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::Persist(PersistCmd::Config(cfg))
            if cfg.animations.master && cfg.animations.radio_master == Some(true)
    )));
}

#[test]
fn palette_context_actions_do_not_type_into_focused_fields() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = app_with_search_results();
    app.search.focus = SearchFocus::Input;
    app.search.input = "hello".to_owned();
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "favorite");
    select_palette(&mut app, |target| {
        matches!(
            target,
            PaletteTarget::ContextAction {
                ctx: KeyContext::SearchResults,
                action: Action::Favorite,
            }
        )
    });
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.search.input, "hello");
    assert!(app.library.is_favorite("a"));
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Library)))
    );

    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for ch in "lov".chars() {
        app.update(Msg::Key(key(KeyCode::Char(ch))));
    }
    assert!(app.library_ui.filter_editing);
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "play whole");
    select_palette(&mut app, |target| {
        matches!(
            target,
            PaletteTarget::ContextAction {
                ctx: KeyContext::Library,
                action: Action::PlayAll,
            }
        )
    });
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.library_ui.filter_query, "lov");
    assert!(!cmds.is_empty(), "play all must run instead of typing 'a'");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|song| song.video_id.as_str()),
        Some("a")
    );

    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.local_dedicated_mode = true;
    app.local_mode.ui.filter_editing = true;
    app.local_mode.ui.filter_query = "jazz".to_owned();
    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "crossfade");
    select_palette(&mut app, |target| {
        matches!(
            target,
            PaletteTarget::ContextAction {
                ctx: KeyContext::LocalDeck,
                action: Action::LocalCrossfadeUp,
            }
        )
    });
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.local_mode.ui.filter_query, "jazz");
    assert!(!app.audio.local_crossfade.is_off());
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn palette_text_zoom_actions_step_zoom() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.zoom.set_mode(crate::zoom::ZoomMode::Osc66);
    assert_eq!(app.zoom.percent(), 100);

    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "text size up");
    select_palette(&mut app, |target| {
        matches!(target, PaletteTarget::GlobalAction(Action::TextZoomIn))
    });
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.overlays.command_palette.is_none());
    assert!(app.zoom.percent() > 100);
    let percent = app.zoom.percent();
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::Persist(PersistCmd::Config(cfg)) if cfg.text_zoom == Some(percent)
    )));
}

#[test]
fn palette_commits_update_open_settings_draft() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert!(app.settings.is_some());
    app.settings.as_mut().unwrap().draft.animations.fps = 12;

    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "theme dracula");
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
    );
    assert_eq!(app.theme.preset_enum(), ThemePreset::Dracula);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.theme.preset_enum(),
        ThemePreset::Dracula
    );
    assert_eq!(app.settings.as_ref().unwrap().draft.animations.fps, 12);

    app.update(Msg::Key(ctrl_k()));
    type_str(&mut app, "matrix rain");
    select_palette(&mut app, |target| {
        matches!(target, PaletteTarget::Animation(Field::AnimRain))
    });
    app.update(Msg::Key(key(KeyCode::Enter)));
    let draft = &app.settings.as_ref().unwrap().draft.animations;
    assert!(draft.rain);
    assert!(draft.master);
    assert_eq!(draft.fps, 12);
    assert!(app.config.animations.rain);
    assert!(app.config.animations.master);

    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.config.theme.preset_enum(), ThemePreset::Dracula);
    assert!(app.config.animations.rain);
    assert!(app.config.animations.master);
    assert_eq!(app.config.animations.fps, 12);
}
