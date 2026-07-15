use super::*;
use crate::util::text_edit::TextCursor;

fn search_with_cursor(input: &str, byte: usize) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    app.search.input = input.to_owned();
    app.search.input_cursor = TextCursor::from_byte_index(byte);
    app
}

fn enable_caret_animation(app: &mut App) {
    app.config.animations.master = true;
    app.config.animations.caret = true;
}

fn assert_live_text_caret(app: &App, editor: &str) {
    assert!(app.in_text_entry(), "{editor} must capture text input");
    assert!(
        app.text_input_caret_visible(),
        "{editor} must expose its visible caret to the animation gate"
    );
    assert!(
        app.animation_active(),
        "{editor} must keep the enabled caret animation clock awake"
    );
}

#[test]
fn search_renders_the_caret_at_a_middle_cursor() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let app = search_with_cursor("alpha beta", "alpha".len());

    let buffer = render_app_buffer(&app, 80, 24);

    assert!(
        buffer_contains(&buffer, "alpha█ beta"),
        "the rendered caret must split the query at the model cursor"
    );
}

#[test]
fn narrow_search_keeps_whole_wide_graphemes_next_to_the_caret() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let app = search_with_cursor("가나다라마바사", "가나다".len());

    let buffer = render_app_buffer(&app, 36, 14);
    let input = app
        .hits
        .rect_of_target(MouseTarget::SearchInput)
        .expect("search input hit target");
    let caret = (input.top()..input.bottom())
        .flat_map(|y| (input.left()..input.right()).map(move |x| (x, y)))
        .find(|&(x, y)| buffer.cell((x, y)).is_some_and(|cell| cell.symbol() == "█"))
        .expect("visible search caret");

    assert!(caret.0 >= input.left() + 2);
    assert_eq!(
        buffer.cell((caret.0 - 2, caret.1)).unwrap().symbol(),
        "다",
        "the complete two-cell grapheme before the caret stays visible"
    );
    assert_eq!(
        buffer.cell((caret.0 + 1, caret.1)).unwrap().symbol(),
        "라",
        "the complete two-cell grapheme after the caret stays visible"
    );
}

#[test]
fn every_overlay_and_local_text_editor_activates_the_caret_clock() {
    let mut local = App::new(100);
    enable_caret_animation(&mut local);
    local.mode = Mode::Library;
    local.local_dedicated_mode = true;
    local.local_mode.ui.filter_editing = true;
    assert_live_text_caret(&local, "Local Deck filter");

    let mut recording = App::new(100);
    enable_caret_animation(&mut recording);
    recording.overlays.recording_settings = Some(RecordingSettingsPopup {
        editing_dir: true,
        ..RecordingSettingsPopup::default()
    });
    assert_live_text_caret(&recording, "recording folder editor");

    let mut audio = App::new(100);
    enable_caret_animation(&mut audio);
    let _ = audio.open_audio_output_picker();
    audio
        .overlays
        .audio_output_picker
        .as_mut()
        .expect("audio output picker")
        .editing_manual = true;
    assert_live_text_caret(&audio, "manual audio device editor");
}
