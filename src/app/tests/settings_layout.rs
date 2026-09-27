//! Settings layout regressions: nothing a focused row or a Sync pane carries may become
//! unreachable at narrow or short sizes. Content taller than the screen must be reachable by
//! scrolling, so the helpers below walk every scroll offset and check each piece of text and
//! each action appears in some frame.

use super::*;

const LANGUAGES: [crate::i18n::Language; 3] = [
    crate::i18n::Language::English,
    crate::i18n::Language::Korean,
    crate::i18n::Language::Japanese,
];

/// Every visible character in reading order, with whitespace, frame borders, and the list
/// scrollbar removed, so text that wrapped across rows reads as one run.
fn compact_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .flat_map(|y| buffer_row(buffer, y).chars().collect::<Vec<_>>())
        .filter(|c| !c.is_whitespace() && !matches!(c, '│' | '█' | '┐' | '┘' | '─'))
        .collect()
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// One rendered frame: its compact text and the mouse targets it published.
type Frame = (String, Vec<MouseTarget>);

fn frame(app: &App, width: u16, height: u16) -> Frame {
    let text = compact_text(&render_app_buffer(app, width, height));
    let targets = app
        .hits
        .regions()
        .iter()
        .map(|r| r.target.clone())
        .collect();
    (text, targets)
}

/// A position the Settings view reports between frames: scroll offset plus every selection
/// the input could move. Input that changes none of these has reached the end.
fn view_state(app: &App) -> (usize, usize, usize, usize) {
    (
        app.bridges.settings_scroll.offset(),
        app.settings.as_ref().map_or(0, |st| st.row),
        app.server.settings.selected,
        app.personal_state.sync_ui.row,
    )
}

/// Render, then feed `input` repeatedly (rendering after each, as the real loop does) until
/// it stops changing the view, collecting every frame on the way.
fn frames_by_input(
    app: &mut App,
    width: u16,
    height: u16,
    mut input: impl FnMut(&mut App),
) -> Vec<Frame> {
    let mut frames = vec![frame(app, width, height)];
    for _ in 0..400 {
        let before = view_state(app);
        input(app);
        frames.push(frame(app, width, height));
        if view_state(app) == before {
            return frames;
        }
    }
    panic!("input never reached the end of the view");
}

fn press(code: KeyCode) -> impl FnMut(&mut App) {
    move |app: &mut App| {
        app.update(Msg::Key(key(code)));
    }
}

fn wheel(up: bool) -> impl FnMut(&mut App) {
    move |app: &mut App| {
        app.update(Msg::MouseScroll {
            up,
            col: 10,
            row: 6,
            ctrl: false,
        });
    }
}

fn wheel_down(app: &mut App) {
    wheel(false)(app);
}

/// Wheel all the way up, then all the way down, as a reader would.
fn frames_by_wheel(app: &mut App, width: u16, height: u16) -> Vec<Frame> {
    let mut frames = frames_by_input(app, width, height, wheel(true));
    frames.extend(frames_by_input(app, width, height, wheel(false)));
    frames
}

/// Draw the pane as the user first sees it, press Home, then page down to the end.
fn frames_by_paging(app: &mut App, width: u16, height: u16) -> Vec<Frame> {
    let _ = frame(app, width, height);
    app.update(Msg::Key(key(KeyCode::Home)));
    frames_by_input(app, width, height, press(KeyCode::PageDown))
}

/// Assert every piece of `text` shows in some frame. Pieces are short enough that a wrap
/// boundary falls between two adjacent rows of one frame.
fn assert_reachable(frames: &[Frame], text: &str, context: &str) {
    let text: Vec<char> = compact(text).chars().collect();
    for piece in text.chunks(6) {
        let piece: String = piece.iter().collect();
        assert!(
            frames.iter().any(|(frame, _)| frame.contains(&piece)),
            "{context}: {piece:?} of {:?} never shown",
            text.iter().collect::<String>()
        );
    }
}

fn assert_clickable(frames: &[Frame], target: MouseTarget, context: &str) {
    assert!(
        frames.iter().any(|(_, targets)| targets.contains(&target)),
        "{context}: {target:?} never clickable"
    );
}

#[test]
fn narrow_settings_show_the_whole_description_in_every_language() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    for language in LANGUAGES {
        crate::i18n::set_language(language);
        focus_settings_field(&mut app, SettingsTab::Ai, Field::RomanizedTitles);
        let text = compact_text(&render_app_buffer(&app, 40, 24));
        assert!(
            text.contains(&compact(Field::RomanizedTitles.description())),
            "{language:?}: description cut off in {text}"
        );
    }
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn a_very_long_path_and_its_description_stay_reachable_at_narrow_sizes() {
    let _guard = crate::i18n::lock_for_test();
    let path = format!(
        "/Users/someone/Music/Collections/Lossless/Archive/{}/Tail-Marker-End",
        "Very-Long-Folder-Name".repeat(12)
    );
    assert!(path.len() > 250);
    let mut app = App::new(100);
    for language in LANGUAGES {
        crate::i18n::set_language(language);
        focus_settings_field(&mut app, SettingsTab::General, Field::LocalMusicRoot);
        app.settings.as_mut().unwrap().draft.local_music_root = path.clone();
        for (width, height) in [(40, 24), (40, 16)] {
            for (how, frames) in [
                (
                    "PageDown",
                    frames_by_input(&mut app, width, height, press(KeyCode::PageDown)),
                ),
                ("wheel", frames_by_wheel(&mut app, width, height)),
            ] {
                let context = format!("{language:?} {width}x{height} {how}");
                assert_reachable(&frames, &path, &context);
                assert_reachable(&frames, Field::LocalMusicRoot.description(), &context);
            }
            focus_settings_field(&mut app, SettingsTab::General, Field::LocalMusicRoot);
        }
    }
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn keyboard_focus_scrolls_the_detail_rows_into_view() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    // The last General field: its detail rows sit at the very end of the list.
    focus_settings_field(&mut app, SettingsTab::General, Field::ResetAll);
    let text = compact_text(&render_app_buffer(&app, 40, 24));
    assert!(
        text.contains(&compact(Field::ResetAll.description())),
        "{text}"
    );
}

#[test]
fn short_sync_panes_reach_every_message_and_action_by_keyboard_and_wheel() {
    let _guard = crate::i18n::lock_for_test();
    for language in LANGUAGES {
        crate::i18n::set_language(language);
        let mut app = App::new(100);
        app.open_settings();
        app.settings.as_mut().unwrap().tab = SettingsTab::Sync;

        // Status has nothing to select: ↓ and the wheel scroll its text to the CLI command.
        app.server.settings.area = crate::app::SyncArea::Status;
        app.server
            .settings
            .summary
            .playback_reports_needing_decision = 2;
        for (width, height) in [(40, 14), (40, 24)] {
            for (how, input) in [
                (
                    "Down",
                    Box::new(press(KeyCode::Down)) as Box<dyn FnMut(&mut App)>,
                ),
                ("wheel", Box::new(wheel_down)),
            ] {
                app.bridges.settings_scroll.reset();
                let context = format!("{language:?} status {width}x{height} {how}");
                let frames = frames_by_input(&mut app, width, height, input);
                assert_reachable(&frames, "ytt server scrobbles list", &context);
            }
        }

        // Music server: the failure, its recovery step, the summary, and every action.
        app.server
            .settings
            .summary
            .playback_reports_needing_decision = 0;
        app.server.settings.summary.configured = true;
        app.server.settings.summary.custom_ca = true;
        app.server.settings.area = crate::app::SyncArea::MusicServer;
        let failure = crate::app::MusicServerFailure::Certificate;
        app.server.settings.failure = Some(failure);
        for (width, height) in [(40, 14), (40, 24)] {
            for how in ["Home+PageDown", "wheel", "Down"] {
                app.server.settings.selected = 0;
                app.bridges.settings_scroll.reset();
                let frames = match how {
                    "Home+PageDown" => frames_by_paging(&mut app, width, height),
                    "wheel" => frames_by_wheel(&mut app, width, height),
                    _ => frames_by_input(&mut app, width, height, press(KeyCode::Down)),
                };
                let context = format!("{language:?} server {width}x{height} {how}");
                if how != "Down" {
                    assert_reachable(&frames, failure.label(), &context);
                    assert_reachable(&frames, failure.recovery_label(), &context);
                    assert_reachable(
                        &frames,
                        crate::t!("Custom CA file", "사용자 CA 파일", "カスタムCAファイル"),
                        &context,
                    );
                }
                for row in 0..4 {
                    assert_clickable(&frames, MouseTarget::SettingsMusicServerRow(row), &context);
                }
            }
        }

        // Personal state, configured and failing: the failure, its recovery step, every row.
        app.server.settings.area = crate::app::SyncArea::PersonalState;
        app.personal_state.sync_ui.lifecycle = crate::sync::service::SyncLifecycleState::Active;
        app.personal_state.sync_ui.status.configured = true;
        app.personal_state.sync_ui.status.state = crate::sync::SyncHealthState::NeedsAttention;
        let sync_failure = crate::sync::SyncFailureKind::Certificate;
        app.personal_state.sync_ui.status.failure = Some(sync_failure);
        assert_eq!(app.sync_settings_model().failure, Some(sync_failure));
        let rows = app.sync_settings_model().rows.len();
        for (width, height) in [(40, 14), (40, 24)] {
            app.personal_state.sync_ui.row = 0;
            app.bridges.settings_scroll.reset();
            let context = format!("{language:?} personal {width}x{height}");
            let mut frames = frames_by_paging(&mut app, width, height);
            frames.extend(frames_by_input(
                &mut app,
                width,
                height,
                press(KeyCode::Down),
            ));
            frames.extend(frames_by_wheel(&mut app, width, height));
            assert_reachable(
                &frames,
                crate::settings::sync::failure_label(sync_failure),
                &context,
            );
            assert_reachable(
                &frames,
                crate::settings::sync::failure_recovery_label(sync_failure),
                &context,
            );
            for row in 0..rows {
                assert_clickable(&frames, MouseTarget::SettingsSyncRow(row), &context);
            }
        }
    }
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn status_footer_names_the_scroll_keys() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = crate::app::SyncArea::Status;
    let (text, _) = frame(&app, 100, 30);
    assert!(text.contains("↑/↓PgUp/PgDnscroll"), "{text}");
    assert!(text.contains("area"), "{text}");

    // A field list that overflows names the page keys too.
    focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
    let (text, _) = frame(&app, 100, 30);
    assert!(text.contains("PgUp/PgDnscroll"), "{text}");
}

#[test]
fn scrolled_off_cursor_is_revealed_before_any_action_and_targets_stay_aligned() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    focus_settings_field(
        &mut app,
        SettingsTab::General,
        Field::LocalMusicRootRecursive,
    );
    let row = app.settings.as_ref().unwrap().row;
    let before = app
        .settings
        .as_ref()
        .unwrap()
        .draft
        .local_music_root_recursive;
    let _ = frame(&app, 40, 16);
    // Scroll the toggle off screen, then press → : it must only come back into view.
    for _ in 0..3 {
        app.update(Msg::Key(key(KeyCode::PageDown)));
        let _ = frame(&app, 40, 16);
    }
    let (_, targets) = frame(&app, 40, 16);
    assert!(
        !targets.contains(&MouseTarget::SettingsChange { row, delta: 1 }),
        "the toggle should be off screen after PageDown"
    );
    app.update(Msg::Key(key(KeyCode::Right)));
    let (_, targets) = frame(&app, 40, 16);
    let st = app.settings.as_ref().unwrap();
    assert_eq!(st.row, row, "selection unchanged by scrolling");
    assert_eq!(
        st.draft.local_music_root_recursive, before,
        "hidden row not toggled"
    );
    assert!(targets.contains(&MouseTarget::SettingsChange { row, delta: 1 }));
    // Now visible, the same key acts on it.
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_ne!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .local_music_root_recursive,
        before
    );
}

#[test]
fn music_server_click_targets_match_their_rows_after_scrolling() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = crate::app::SyncArea::MusicServer;
    app.server.settings.summary.configured = true;
    app.server.settings.failure = Some(crate::app::MusicServerFailure::Certificate);
    let labels = [
        "Test connection",
        "Edit connection",
        "detailed history",
        "Remove server",
    ];
    let _ = frame(&app, 40, 14);
    for _ in 0..6 {
        app.update(Msg::Key(key(KeyCode::PageDown)));
        let buffer = render_app_buffer(&app, 40, 14);
        for region in app.hits.regions().iter() {
            if let MouseTarget::SettingsMusicServerRow(index) = region.target {
                let text = buffer_row(&buffer, region.rect.y);
                assert!(
                    text.contains(labels[index]),
                    "row {index} target sits on {text:?}"
                );
            }
        }
    }
    assert_eq!(
        app.server.settings.selected, 0,
        "paging never moves the selection"
    );
}

#[test]
fn tab_switch_and_resize_after_scrolling_keep_the_view_sane() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
    let _ = frame(&app, 40, 16);
    app.update(Msg::Key(key(KeyCode::PageDown)));
    app.update(Msg::Key(key(KeyCode::PageDown)));
    let _ = frame(&app, 40, 16);
    assert!(app.bridges.settings_scroll.offset() > 0);

    // A resize brings the scrolled-off cursor back.
    let (_, targets) = frame(&app, 60, 20);
    assert!(targets.contains(&MouseTarget::SettingsChange { row: 0, delta: 1 }));

    // Switching tabs starts the next tab at its top, cursor visible.
    app.update(Msg::Key(key(KeyCode::PageDown)));
    app.update(Msg::Key(key(KeyCode::Tab)));
    let (text, targets) = frame(&app, 40, 16);
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Playback);
    assert!(text.contains("NowPlaying"), "{text}");
    assert!(targets.contains(&MouseTarget::SettingsChange { row: 0, delta: -1 }));
}

#[test]
fn a_long_secret_stays_masked_in_the_detail_rows() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    focus_settings_field(&mut app, SettingsTab::Ai, Field::ApiKey);
    app.settings.as_mut().unwrap().draft.gemini_api_key = "AIzaSECRET".repeat(8);
    let frames = frames_by_input(&mut app, 40, 16, press(KeyCode::PageDown));
    assert!(frames.iter().all(|(text, _)| !text.contains("SECRET")));
}

#[test]
fn retro_sync_panes_render_console_safe_glyphs() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.retro_mode = true;
    app.open_settings();
    assert!(app.retro_mode());
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.summary.configured = true;
    app.server.settings.failure = Some(crate::app::MusicServerFailure::Certificate);
    for area in crate::app::SyncArea::ALL {
        app.server.settings.area = area;
        let text = compact_text(&render_app_buffer(&app, 80, 24));
        assert!(
            !text.contains('●') && !text.contains('›'),
            "{area:?}: {text}"
        );
        assert!(text.contains('•'), "{area:?}: status dot missing in {text}");
    }
}

#[test]
fn short_music_server_setup_keeps_every_field_and_save_reachable_and_masks_the_secret() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.open_settings();
    let mut form = crate::app::MusicServerSetupForm::default();
    form.secret = zeroize::Zeroizing::new("hunter2-hunter2".to_owned());
    app.server.settings.wizard = Some(crate::app::MusicServerWizard::Setup(form));
    let fields = crate::app::MusicServerSetupField::ALL.len();
    for height in [12, 14, 24] {
        // Keyboard focus walks every field; each one is clickable while focused.
        for index in 0..fields {
            let Some(crate::app::MusicServerWizard::Setup(form)) =
                app.server.settings.wizard.as_mut()
            else {
                panic!("setup form");
            };
            form.selected = index;
            let buffer = render_app_buffer(&app, 44, height);
            assert!(
                !compact_text(&buffer).contains("hunter2"),
                "secret shown at height {height}"
            );
            assert!(
                app.hits
                    .regions()
                    .iter()
                    .any(|region| region.target == MouseTarget::MusicServerWizardField(index)),
                "field {index} not reachable at height {height}"
            );
        }
    }
}

#[test]
fn music_server_list_follows_remapped_move_and_confirm_keys() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    for (action, chord) in [
        (Action::MoveDown, "f5"),
        (Action::MoveUp, "f6"),
        (Action::Confirm, "f7"),
    ] {
        app.keymap
            .rebind(
                KeyContext::Common,
                action,
                crate::keymap::parse_chord(chord).unwrap(),
            )
            .unwrap();
    }
    app.open_settings();
    app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
    app.server.settings.area = crate::app::SyncArea::MusicServer;
    let _ = frame(&app, 80, 24);

    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.server.settings.selected, 1, "remapped MoveDown");
    app.update(Msg::Key(key(KeyCode::F(6))));
    assert_eq!(app.server.settings.selected, 0, "remapped MoveUp");
    // The literal arrows stay as the safety keys.
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.server.settings.selected, 1, "safety Down");
    app.update(Msg::Key(key(KeyCode::Up)));
    // Remapped Confirm on "Set up music server" opens the setup form.
    app.update(Msg::Key(key(KeyCode::F(7))));
    assert!(
        matches!(
            app.server.settings.wizard,
            Some(crate::app::MusicServerWizard::Setup(_))
        ),
        "remapped Confirm"
    );
    // The form's own editing keys still work: typing lands in the focused Name field.
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let Some(crate::app::MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_ref()
    else {
        panic!("setup form");
    };
    assert_eq!(form.display_name.as_str(), "n");
}

#[test]
fn narrow_footer_keeps_every_essential_hint_in_every_language() {
    let _guard = crate::i18n::lock_for_test();
    for language in LANGUAGES {
        crate::i18n::set_language(language);
        let mut app = App::new(100);
        let area = format!("←/→{}", crate::t!("area", "영역", "エリア"));
        let tab = format!(
            "Tab{}",
            compact(crate::t!("switch tab", "탭 전환", "タブ切替"))
        );
        let close = format!("q{}", crate::t!("close", "닫기", "閉じる"));
        let quit = format!(
            "q{}",
            compact(crate::t!("save + quit", "저장하고 닫기", "保存して閉じる"))
        );
        let open = format!("Enter{}", crate::t!("open", "열기", "開く"));
        let edit = format!(
            "Enter{}",
            crate::t!("edit/toggle", "편집/전환", "編集/切替")
        );
        let keys = format!("?{}", compact(crate::t!("all keys", "전체 키", "全キー")));

        focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
        let mut cases: Vec<(&str, Vec<String>)> = vec![(
            "General",
            vec![edit.clone(), tab.clone(), quit.clone(), keys.clone()],
        )];
        let mut sync_cases = vec![
            (
                crate::app::SyncArea::Status,
                "Status",
                vec![
                    "PgUp/PgDn".to_owned(),
                    area.clone(),
                    tab.clone(),
                    close.clone(),
                ],
            ),
            (
                crate::app::SyncArea::MusicServer,
                "Music server",
                vec![open.clone(), area.clone(), tab.clone(), close.clone()],
            ),
            (
                crate::app::SyncArea::PersonalState,
                "Personal state",
                vec![open.clone(), area.clone(), tab.clone(), close.clone()],
            ),
        ];
        let footer_of = |app: &App| {
            let buffer = render_app_buffer(app, 40, 24);
            // The footer is the last one or two rows above the bottom border; drop the frame
            // and collapse-toggle cells so a hint wrapped across the two rows reads as one run.
            compact(&format!(
                "{}{}",
                buffer_row(&buffer, 21),
                buffer_row(&buffer, 22)
            ))
            .chars()
            .filter(|c| !matches!(c, '│' | '▼' | '▲'))
            .collect::<String>()
        };
        // The export privacy warning is never cut short, even though it must wrap.
        focus_settings_field(&mut app, SettingsTab::General, Field::ExportPersonalData);
        let export = footer_of(&app);
        let warning = compact(crate::t!(
            "unencrypted JSON · includes private listening history",
            "암호화되지 않은 JSON · 개인 감상 기록 포함",
            "暗号化されないJSON · 個人の再生履歴を含む"
        ));
        assert!(
            export.contains(&warning),
            "{language:?}: {warning} in {export}"
        );
        focus_settings_field(&mut app, SettingsTab::General, Field::BeginnerMode);
        let general = footer_of(&app);
        for (name, needles) in cases.drain(..) {
            for needle in needles {
                assert!(
                    general.contains(&needle),
                    "{language:?} {name}: {needle} in {general}"
                );
            }
        }
        app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
        for (sync_area, name, needles) in sync_cases.drain(..) {
            app.server.settings.area = sync_area;
            let footer = footer_of(&app);
            for needle in needles {
                assert!(
                    footer.contains(&needle),
                    "{language:?} {name}: {needle} in {footer}"
                );
            }
        }
    }
    crate::i18n::set_language(crate::i18n::Language::English);
}

#[test]
fn help_key_opens_the_full_key_list_from_settings() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.open_settings();
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert!(app.overlays.help_visible);
}
