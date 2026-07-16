use super::*;

fn ev(code: KeyCode, mods: KeyModifiers) -> Chord {
    Chord::from(KeyEvent::new(code, mods))
}

#[test]
fn space_formats_as_symbol() {
    assert_eq!(format_chord(parse_chord("space").unwrap()), "␣");
    assert_eq!(
        chord_to_config(Chord::new(KeyCode::Char(' '), KeyModifiers::empty())),
        "space"
    );
}

#[test]
fn ctrl_and_arrow_formatting() {
    assert_eq!(format_chord(parse_chord("ctrl+r").unwrap()), "^r");
    assert_eq!(format_chord(parse_chord("ctrl+backspace").unwrap()), "^⌫");
    assert_eq!(format_chord(parse_chord("ctrl+shift+x").unwrap()), "^⇧x");
    assert_eq!(format_chord(parse_chord("ctrl+q").unwrap()), "^q");
    assert_eq!(format_chord(parse_chord("ctrl+h").unwrap()), "^h");
    assert_eq!(format_chord(parse_chord("left").unwrap()), "←");
    assert_eq!(format_chord(parse_chord("right").unwrap()), "→");
    assert_eq!(format_chord(parse_chord("up").unwrap()), "↑");
    assert_eq!(format_chord(parse_chord("down").unwrap()), "↓");
    assert_eq!(chord_to_config(parse_chord("ctrl+r").unwrap()), "ctrl+r");
    assert_eq!(
        chord_to_config(parse_chord("ctrl+shift+x").unwrap()),
        "ctrl+shift+x"
    );
}

#[test]
fn inherent_uppercase_chords_display_as_shifted_without_changing_config() {
    let chord = parse_chord("L").unwrap();
    assert_eq!(format_chord(chord), "⇧L");
    assert_eq!(format_chord_retro(chord), "Shift+L");
    assert_eq!(chord_to_config(chord), "L");
    assert_eq!(format_chord(parse_chord("?").unwrap()), "?");
    assert_eq!(format_chord(parse_chord("ctrl+shift+x").unwrap()), "^⇧x");
}

#[test]
fn retro_key_labels_use_words_and_plus_separators() {
    assert_eq!(format_chord_retro(parse_chord("space").unwrap()), "Space");
    assert_eq!(format_chord_retro(parse_chord("ctrl+r").unwrap()), "Ctrl+R");
    assert_eq!(
        format_chord_retro(parse_chord("ctrl+shift+x").unwrap()),
        "Ctrl+Shift+X"
    );
    assert_eq!(
        format_chord_retro(parse_chord("alt+shift+r").unwrap()),
        "Alt+Shift+R"
    );
    assert_eq!(format_chord_retro(parse_chord("left").unwrap()), "Left");
    assert_eq!(format_chord_retro(parse_chord("right").unwrap()), "Right");
    assert_eq!(format_chord_retro(parse_chord("up").unwrap()), "Up");
    assert_eq!(format_chord_retro(parse_chord("down").unwrap()), "Down");
    assert_eq!(
        format_chord_retro(parse_chord("backtab").unwrap()),
        "Shift+Tab"
    );
    assert_eq!(
        format_chord_for_display(parse_chord("space").unwrap(), true),
        "Space"
    );
    assert_eq!(
        format_chord_for_display(parse_chord("space").unwrap(), false),
        "␣"
    );
}

#[test]
fn parse_format_round_trip() {
    for s in [
        "space",
        "ctrl+n",
        "ctrl+q",
        "ctrl+h",
        "ctrl+shift+x",
        "alt+shift+1",
        "L",
        ">",
        "/",
        "?",
        "enter",
        "esc",
        "backtab",
        "f5",
        "capslock",
        "printscreen",
        "media_play_pause",
        "left_shift",
    ] {
        let chord = parse_chord(s).unwrap();
        assert_eq!(
            parse_chord(&chord_to_config(chord)).unwrap(),
            chord,
            "round trip {s}"
        );
    }
}

#[test]
fn shift_is_normalized_for_chars() {
    // Shift+L (uppercase char, with or without the SHIFT flag) is one chord.
    let a = ev(KeyCode::Char('L'), KeyModifiers::SHIFT);
    let b = ev(KeyCode::Char('L'), KeyModifiers::empty());
    assert_eq!(a, b);
    assert_eq!(
        parse_chord("shift+l").unwrap(),
        Chord::new(KeyCode::Char('L'), KeyModifiers::empty())
    );
    assert_eq!(chord_to_config(parse_chord("shift+l").unwrap()), "L");
    // Shift+Tab collapses to BackTab.
    assert_eq!(
        ev(KeyCode::Tab, KeyModifiers::SHIFT),
        ev(KeyCode::BackTab, KeyModifiers::empty())
    );
}

#[test]
fn ctrl_char_case_is_normalized() {
    assert_eq!(
        ev(KeyCode::Char('Q'), KeyModifiers::CONTROL),
        ev(KeyCode::Char('q'), KeyModifiers::CONTROL)
    );
    assert_eq!(
        chord_to_config(ev(KeyCode::Char('Q'), KeyModifiers::CONTROL)),
        "ctrl+q"
    );
}

#[test]
fn ctrl_shift_char_chords_stay_distinct() {
    let ctrl_x = parse_chord("ctrl+x").unwrap();
    let ctrl_shift_x = parse_chord("ctrl+shift+x").unwrap();
    assert_ne!(ctrl_x, ctrl_shift_x);
    assert_eq!(
        ev(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ),
        ctrl_shift_x
    );
    assert_eq!(
        ev(
            KeyCode::Char('X'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ),
        ctrl_shift_x
    );
    // Some terminals report Ctrl+X as an uppercase char without the Shift modifier.
    assert_eq!(ev(KeyCode::Char('X'), KeyModifiers::CONTROL), ctrl_x);

    let mut km = KeyMap::default();
    km.rebind(KeyContext::Player, Action::TogglePause, ctrl_shift_x)
        .unwrap();
    assert_eq!(
        km.action(KeyContext::Player, ctrl_shift_x),
        Some(Action::TogglePause)
    );
    assert_eq!(km.action(KeyContext::Player, ctrl_x), None);
}

#[test]
fn korean_2set_keys_normalize_to_qwerty() {
    assert_eq!(
        ev(KeyCode::Char('ㅂ'), KeyModifiers::empty()),
        parse_chord("q").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅂ'), KeyModifiers::CONTROL),
        parse_chord("ctrl+q").unwrap()
    );
    assert_eq!(
        ev(
            KeyCode::Char('ㅂ'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ),
        parse_chord("ctrl+shift+q").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅃ'), KeyModifiers::CONTROL),
        parse_chord("ctrl+shift+q").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㄱ'), KeyModifiers::CONTROL),
        parse_chord("ctrl+r").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅂ'), KeyModifiers::ALT),
        parse_chord("alt+q").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅣ'), KeyModifiers::SHIFT),
        parse_chord("L").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅇ'), KeyModifiers::SHIFT),
        parse_chord("D").unwrap()
    );
    assert_eq!(
        ev(KeyCode::Char('ㅆ'), KeyModifiers::empty()),
        parse_chord("T").unwrap()
    );
}

#[test]
fn shifted_korean_2set_keys_without_distinct_jamo_normalize_to_uppercase_qwerty() {
    for (jamo, latin) in [
        ('ㅛ', 'Y'),
        ('ㅕ', 'U'),
        ('ㅑ', 'I'),
        ('ㅁ', 'A'),
        ('ㄴ', 'S'),
        ('ㅇ', 'D'),
        ('ㄹ', 'F'),
        ('ㅎ', 'G'),
        ('ㅗ', 'H'),
        ('ㅓ', 'J'),
        ('ㅏ', 'K'),
        ('ㅣ', 'L'),
        ('ㅋ', 'Z'),
        ('ㅌ', 'X'),
        ('ㅊ', 'C'),
        ('ㅍ', 'V'),
        ('ㅠ', 'B'),
        ('ㅜ', 'N'),
        ('ㅡ', 'M'),
    ] {
        assert_eq!(
            ev(KeyCode::Char(jamo), KeyModifiers::SHIFT),
            Chord::new(KeyCode::Char(latin), KeyModifiers::empty()),
            "{jamo} should normalize to {latin}",
        );
    }
}

#[test]
fn default_bindings_are_conflict_free() {
    // The default table is hand-maintained. The rebind API rejects conflicts, but
    // nothing checks the table itself: a duplicate (context, chord) would silently
    // shadow one action on lookup, and a duplicate (context, action) would silently
    // drop the earlier chord when `from_labels` collects into its HashMap.
    let mut by_chord = HashMap::new();
    let mut by_action = HashMap::new();
    for (ctx, action, chord) in default_bindings() {
        if let Some(prev) = by_chord.insert((ctx, chord), action) {
            panic!(
                "{ctx:?}: `{}` bound to both {prev:?} and {action:?}",
                format_chord(chord)
            );
        }
        if let Some(prev) = by_action.insert((ctx, action), chord) {
            panic!(
                "{ctx:?} {action:?}: bound to both `{}` and `{}`",
                format_chord(prev),
                format_chord(chord)
            );
        }
    }
    // Global chords are consulted before every per-screen handler, so a non-Global
    // default reusing one is dead at runtime unless dispatch deliberately gives a
    // local context first claim. Mirror the asymmetric conflict definition of
    // `KeyMap::conflict` (Common shadowing, by contrast, is deliberate).
    for (ctx, action, chord) in default_bindings() {
        if ctx == KeyContext::Global {
            continue;
        }
        if let Some(&global) = by_chord.get(&(KeyContext::Global, chord)) {
            let local_deck_accept_all_shadows_global_animation = ctx == KeyContext::LocalDeck
                && action == Action::AcceptAllImportReview
                && global == Action::ToggleAnimations
                && chord == Chord::new(KeyCode::Char('A'), KeyModifiers::empty());
            if local_deck_accept_all_shadows_global_animation {
                continue;
            }
            panic!(
                "{ctx:?} {action:?}: `{}` is shadowed by Global {global:?}",
                format_chord(chord)
            );
        }
    }
}

#[test]
fn defaults_resolve_to_actions() {
    let km = KeyMap::default();
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("space").unwrap()),
        Some(Action::TogglePause)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("up").unwrap()),
        Some(Action::VolUp)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("down").unwrap()),
        Some(Action::VolDown)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord(".").unwrap()),
        Some(Action::NextTrack)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord(",").unwrap()),
        Some(Action::PrevTrack)
    );
    assert_eq!(
        km.action(KeyContext::MpvOverlay, parse_chord("space").unwrap()),
        Some(Action::VideoTogglePause)
    );
    assert_eq!(
        km.action(KeyContext::MpvOverlay, parse_chord(".").unwrap()),
        Some(Action::VideoNext)
    );
    assert_eq!(
        km.action(KeyContext::MpvOverlay, parse_chord(",").unwrap()),
        Some(Action::VideoPrev)
    );
    assert_eq!(
        km.action(KeyContext::MpvOverlay, parse_chord("q").unwrap()),
        Some(Action::VideoClose)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("m").unwrap()),
        Some(Action::ToggleMute)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("]").unwrap()),
        Some(Action::SpeedUp)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("[").unwrap()),
        Some(Action::SpeedDown)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("o").unwrap()),
        Some(Action::OpenSettings)
    );
    // `p`/`n` are no longer Player transport keys (freed by the mpv remap).
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("n").unwrap()),
        None
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("p").unwrap()),
        None
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("s").unwrap()),
        Some(Action::OpenSearch)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("x").unwrap()),
        Some(Action::ToggleShuffle)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("S").unwrap()),
        None
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("l").unwrap()),
        Some(Action::OpenLibrary)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("L").unwrap()),
        Some(Action::ToggleLyrics)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("z").unwrap()),
        Some(Action::LyricsDelayEarlier)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("Z").unwrap()),
        Some(Action::LyricsDelayLater)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("delete").unwrap()),
        Some(Action::QueueRemove)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("alt+shift+r").unwrap()),
        Some(Action::ToggleRadioMode)
    );
    assert_eq!(km.global_action(parse_chord("alt+shift+r").unwrap()), None);
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("alt+shift+r").unwrap()),
        None
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("alt+shift+l").unwrap()),
        Some(Action::ToggleLocalMode)
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("alt+shift+l").unwrap()),
        None
    );
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("q").unwrap()),
        Some(Action::Back)
    );
    // Common nav falls through in a list context.
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("up").unwrap()),
        Some(Action::MoveUp)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("down").unwrap()),
        Some(Action::MoveDown)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("q").unwrap()),
        Some(Action::Back)
    );
    // `d` downloads the selected track; `Shift+D` (uppercase, no modifier) downloads the
    // whole list/playlist — distinct bindings in both list contexts.
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("d").unwrap()),
        Some(Action::Download)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("D").unwrap()),
        Some(Action::DownloadAll)
    );
    assert_eq!(
        km.action(KeyContext::Playlists, parse_chord("D").unwrap()),
        Some(Action::DownloadAll)
    );
    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("q").unwrap()),
        Some(Action::Back)
    );
    // `/` filters in every browse context: inline in the Library, popup in Search.
    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("/").unwrap()),
        Some(Action::SearchFilter)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("/").unwrap()),
        Some(Action::LibraryFilter)
    );
    assert_eq!(
        km.global_action(parse_chord("ctrl+q").unwrap()),
        Some(Action::Quit)
    );
    assert_eq!(
        km.global_action(parse_chord("ctrl+h").unwrap()),
        Some(Action::Home)
    );
    assert_eq!(
        km.global_action(parse_chord("?").unwrap()),
        Some(Action::ToggleHelp)
    );
}

#[test]
fn page_and_jump_keys_resolve_in_list_contexts() {
    let km = KeyMap::default();
    // The four new nav primitives live in Common, so they fall through into any list
    // context (Library, Search results, …) and onto their standard physical keys.
    for ctx in [KeyContext::Library, KeyContext::SearchResults] {
        assert_eq!(
            km.action(ctx, parse_chord("pageup").unwrap()),
            Some(Action::PageUp)
        );
        assert_eq!(
            km.action(ctx, parse_chord("pagedown").unwrap()),
            Some(Action::PageDown)
        );
        assert_eq!(
            km.action(ctx, parse_chord("home").unwrap()),
            Some(Action::JumpTop)
        );
        assert_eq!(
            km.action(ctx, parse_chord("end").unwrap()),
            Some(Action::JumpBottom)
        );
    }
    // They round-trip through ids/labels like every other action.
    for a in [
        Action::PageUp,
        Action::PageDown,
        Action::JumpTop,
        Action::JumpBottom,
    ] {
        assert_ne!(a.id(), "?");
        assert_ne!(a.human_label(), "?");
    }
}

#[test]
fn legacy_ctrl_backspace_fallback_tracks_effective_keymap() {
    let ctrl_h = parse_chord("ctrl+h").unwrap();

    let mut km = KeyMap::default();
    assert!(km.legacy_ctrl_backspace_fallback_active());

    km.rebind(
        KeyContext::Common,
        Action::DeleteWord,
        parse_chord("f8").unwrap(),
    )
    .unwrap();
    assert!(!km.legacy_ctrl_backspace_fallback_active());
    km.reset(KeyContext::Common, Action::DeleteWord).unwrap();
    assert!(km.legacy_ctrl_backspace_fallback_active());
    km.unbind(KeyContext::Common, Action::DeleteWord);
    assert!(!km.legacy_ctrl_backspace_fallback_active());

    let mut home_remapped = KeyMap::default();
    home_remapped
        .rebind(KeyContext::Global, Action::Home, parse_chord("f8").unwrap())
        .unwrap();
    assert!(home_remapped.legacy_ctrl_backspace_fallback_active());
    home_remapped.unbind(KeyContext::Global, Action::Home);
    assert!(home_remapped.legacy_ctrl_backspace_fallback_active());

    let mut common_claim = KeyMap::default();
    common_claim
        .rebind(KeyContext::Global, Action::Home, parse_chord("f8").unwrap())
        .unwrap();
    common_claim
        .rebind(KeyContext::Common, Action::MoveUp, ctrl_h)
        .unwrap();
    assert!(!common_claim.legacy_ctrl_backspace_fallback_active());

    let mut global_claim = KeyMap::default();
    global_claim
        .rebind(KeyContext::Global, Action::Home, parse_chord("f8").unwrap())
        .unwrap();
    global_claim
        .rebind(KeyContext::Global, Action::ToggleAbout, ctrl_h)
        .unwrap();
    assert!(!global_claim.legacy_ctrl_backspace_fallback_active());
}

#[test]
fn shift_nav_resolves_to_select_actions() {
    let km = KeyMap::default();
    // Shift+nav lives in Common, so it falls through into the list contexts that act on
    // it (Library, Queue) while staying distinct from the plain nav chords.
    for ctx in [KeyContext::Library, KeyContext::Queue] {
        assert_eq!(
            km.action(ctx, parse_chord("shift+up").unwrap()),
            Some(Action::SelectUp)
        );
        assert_eq!(
            km.action(ctx, parse_chord("shift+down").unwrap()),
            Some(Action::SelectDown)
        );
        assert_eq!(
            km.action(ctx, parse_chord("shift+pageup").unwrap()),
            Some(Action::SelectPageUp)
        );
        assert_eq!(
            km.action(ctx, parse_chord("shift+pagedown").unwrap()),
            Some(Action::SelectPageDown)
        );
        assert_eq!(
            km.action(ctx, parse_chord("shift+home").unwrap()),
            Some(Action::SelectToTop)
        );
        assert_eq!(
            km.action(ctx, parse_chord("shift+end").unwrap()),
            Some(Action::SelectToBottom)
        );
        // Plain nav is untouched — Shift didn't shadow it.
        assert_eq!(
            km.action(ctx, parse_chord("up").unwrap()),
            Some(Action::MoveUp)
        );
    }
    for a in [
        Action::SelectUp,
        Action::SelectDown,
        Action::SelectPageUp,
        Action::SelectPageDown,
        Action::SelectToTop,
        Action::SelectToBottom,
    ] {
        assert_ne!(a.id(), "?");
        assert_ne!(a.human_label(), "?");
    }
}

#[test]
fn korean_2set_keys_resolve_default_actions() {
    let km = KeyMap::default();
    assert_eq!(
        km.action(
            KeyContext::Player,
            ev(KeyCode::Char('ㅂ'), KeyModifiers::empty())
        ),
        Some(Action::Back)
    );
    assert_eq!(
        km.action(
            KeyContext::Player,
            ev(KeyCode::Char('ㅣ'), KeyModifiers::empty())
        ),
        Some(Action::OpenLibrary)
    );
    assert_eq!(
        km.action(
            KeyContext::Player,
            ev(KeyCode::Char('ㅣ'), KeyModifiers::SHIFT)
        ),
        Some(Action::ToggleLyrics)
    );
    assert_eq!(
        km.action(
            KeyContext::Player,
            ev(KeyCode::Char('ㅇ'), KeyModifiers::empty())
        ),
        Some(Action::Download)
    );
    assert_eq!(
        km.action(
            KeyContext::SearchResults,
            ev(KeyCode::Char('ㅂ'), KeyModifiers::empty())
        ),
        Some(Action::Back)
    );
    assert_eq!(
        km.global_action(ev(KeyCode::Char('ㅂ'), KeyModifiers::CONTROL)),
        Some(Action::Quit)
    );
    assert_eq!(
        km.global_action(ev(KeyCode::Char('ㅗ'), KeyModifiers::CONTROL)),
        Some(Action::Home)
    );
    assert_eq!(
        km.global_action(ev(KeyCode::Char('ㄱ'), KeyModifiers::CONTROL)),
        Some(Action::ToggleStreaming)
    );
}

#[test]
fn mpv_input_conversion_covers_overlay_defaults_and_named_keys() {
    let km = KeyMap::default();
    for (action, expected) in [
        (Action::VideoTogglePause, "SPACE"),
        (Action::VideoNext, "."),
        (Action::VideoPrev, ","),
        (Action::VideoClose, "q"),
        (Action::VideoToggleFullscreen, "f"),
        (Action::VideoToggleMute, "m"),
    ] {
        let chord = km.chord(KeyContext::MpvOverlay, action).unwrap();
        assert_eq!(chord_to_mpv_input(chord).as_deref(), Some(expected));
    }

    assert_eq!(
        chord_to_mpv_input(parse_chord("esc").unwrap()).as_deref(),
        Some("ESC")
    );
    assert_eq!(
        chord_to_mpv_input(parse_chord("ctrl+alt+right").unwrap()).as_deref(),
        Some("Ctrl+Alt+RIGHT")
    );
    assert_eq!(
        chord_to_mpv_input(parse_chord("shift+tab").unwrap()).as_deref(),
        Some("Shift+TAB")
    );
    assert_eq!(
        chord_to_mpv_input(parse_chord("f12").unwrap()).as_deref(),
        Some("F12")
    );
}

#[test]
fn mpv_input_conversion_rejects_terminal_only_keys() {
    assert!(chord_to_mpv_input(Chord::new(KeyCode::Null, KeyModifiers::empty())).is_none());
    assert!(
        chord_to_mpv_input(Chord::new(
            KeyCode::Media(MediaKeyCode::PlayPause),
            KeyModifiers::empty(),
        ))
        .is_none()
    );
    assert!(
        chord_to_mpv_input(Chord::new(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::empty(),
        ))
        .is_none()
    );
}

#[test]
fn mpv_input_conversion_uses_normalized_korean_chords() {
    let chord = ev(KeyCode::Char('ㅁ'), KeyModifiers::NONE);
    assert_eq!(chord_to_mpv_input(chord).as_deref(), Some("a"));
}

#[test]
fn contextual_labels_describe_close_and_global_targets() {
    let _guard = crate::i18n::lock_for_test();
    assert_eq!(
        Action::Back.human_label_for(KeyContext::Library),
        "Close Library"
    );
    assert_eq!(
        Action::Confirm.human_label_for(KeyContext::SearchInput),
        "Search"
    );
    assert_eq!(
        Action::Confirm.human_label_for(KeyContext::SearchResults),
        "Play selected"
    );
    assert_eq!(
        Action::FocusPrev.human_label_for(KeyContext::SearchInput),
        "Focus search results"
    );
    assert_eq!(
        Action::FocusPrev.human_label_for(KeyContext::SearchResults),
        "Focus search box"
    );
    assert_eq!(
        Action::Back.human_label_for(KeyContext::SearchResults),
        "Close Search Results"
    );
    assert_eq!(
        Action::SettingsCancel.human_label_for(KeyContext::Settings),
        "Save + quit"
    );
    assert_eq!(
        Action::QueueRemove.human_label_for(KeyContext::Player),
        "Remove current from queue"
    );
    assert_eq!(
        Action::QueueRemove.human_label_for(KeyContext::Queue),
        "Remove selected from queue"
    );
    assert_eq!(Action::Quit.human_label_for(KeyContext::Global), "Quit");
    assert_eq!(Action::Home.human_label_for(KeyContext::Global), "Go home");
}

#[test]
fn enter_backslash_and_play_all_resolve_in_library_and_search_results() {
    let _guard = crate::i18n::lock_for_test();
    let km = KeyMap::default();
    // Library: Enter = play selected, `\` = add to queue, `a` = play the whole tab.
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("enter").unwrap()),
        Some(Action::Confirm)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("\\").unwrap()),
        Some(Action::Enqueue)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("a").unwrap()),
        Some(Action::PlayAll)
    );
    // Search results: `\` = add to queue (Enter stays fixed in the handler, not the keymap).
    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("\\").unwrap()),
        Some(Action::Enqueue)
    );
    assert_eq!(
        km.action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("s").unwrap()),
        Some(Action::FocusInput)
    );
    // The unified play/queue labels read consistently across both surfaces.
    assert_eq!(
        Action::Confirm.human_label_for(KeyContext::Library),
        "Play selected"
    );
    assert_eq!(Action::Enqueue.human_label(), "Add to queue");
    assert_eq!(Action::PlayAll.human_label(), "Play whole tab");
}

#[test]
fn settings_close_binding_is_last_in_group() {
    let settings_actions = groups()
        .into_iter()
        .find_map(|(ctx, actions)| (ctx == KeyContext::Settings).then_some(actions))
        .unwrap();
    assert_eq!(settings_actions.last(), Some(&Action::SettingsCancel));
}

#[test]
fn settings_has_no_standalone_save_binding() {
    let km = KeyMap::default();
    assert_eq!(
        km.action(KeyContext::Settings, parse_chord("s").unwrap()),
        None
    );

    let mut o = BTreeMap::new();
    o.insert("settings.settings_save".to_owned(), "S".to_owned());
    let km = KeyMap::from_overrides(&o);
    assert_eq!(
        km.action(KeyContext::Settings, parse_chord("S").unwrap()),
        None
    );
}

#[test]
fn typeable_detection() {
    assert!(parse_chord("a").unwrap().is_typeable());
    assert!(parse_chord("?").unwrap().is_typeable());
    assert!(!parse_chord("ctrl+r").unwrap().is_typeable());
    assert!(!parse_chord("enter").unwrap().is_typeable());
}

#[test]
fn ctrl_a_selects_all_in_text_inputs() {
    let km = KeyMap::default();
    let ctrl_a = parse_chord("ctrl+a").unwrap();
    assert_eq!(
        km.action(KeyContext::SearchInput, ctrl_a),
        Some(Action::SelectAll)
    );
    assert_eq!(
        km.action(KeyContext::AiInput, ctrl_a),
        Some(Action::SelectAll)
    );
    // Ctrl+A isn't a typed character, so it won't leak into the field as text.
    assert!(!ctrl_a.is_typeable());
}

#[test]
fn rebind_rejects_conflict() {
    let mut km = KeyMap::default();
    // `q` is already Back in Player → binding TogglePause to it is rejected, and the
    // rejection names the offending chord, the action holding it, and where.
    let q = parse_chord("q").unwrap();
    let err = km
        .rebind(KeyContext::Player, Action::TogglePause, q)
        .unwrap_err();
    assert_eq!(err.existing, Action::Back);
    assert_eq!(err.chord, q);
    assert_eq!(err.ctx, KeyContext::Player);
    // Space is still pause; q is still back/close.
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("space").unwrap()),
        Some(Action::TogglePause)
    );
    assert_eq!(km.action(KeyContext::Player, q), Some(Action::Back));
}

#[test]
fn rebind_moves_binding() {
    let mut km = KeyMap::default();
    let p_upper = parse_chord("f5").unwrap();
    km.rebind(KeyContext::Player, Action::TogglePause, p_upper)
        .unwrap();
    assert_eq!(
        km.action(KeyContext::Player, p_upper),
        Some(Action::TogglePause)
    );
    // The old space binding is gone.
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("space").unwrap()),
        None
    );
}

#[test]
fn local_mode_toggle_is_rebindable_in_library_context() {
    let mut km = KeyMap::default();
    let f8 = parse_chord("f8").unwrap();

    km.rebind(KeyContext::Library, Action::ToggleLocalMode, f8)
        .unwrap();

    assert_eq!(
        km.action(KeyContext::Library, f8),
        Some(Action::ToggleLocalMode)
    );
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("alt+shift+l").unwrap()),
        None
    );
    assert_eq!(
        km.to_overrides().get("library.toggle_local_mode"),
        Some(&"f8".to_owned())
    );
}

#[test]
fn local_rebind_can_shadow_common_navigation() {
    let mut km = KeyMap::default();
    let page_up = parse_chord("pageup").unwrap();

    km.rebind(KeyContext::Player, Action::TogglePause, page_up)
        .unwrap();

    assert_eq!(
        km.action(KeyContext::Player, page_up),
        Some(Action::TogglePause)
    );
    assert_eq!(
        km.action(KeyContext::Library, page_up),
        Some(Action::PageUp)
    );
}

#[test]
fn common_rebind_can_be_shadowed_by_player_binding() {
    let mut km = KeyMap::default();
    let dot = parse_chord(".").unwrap();

    km.rebind(KeyContext::Common, Action::PageDown, dot)
        .unwrap();

    assert_eq!(km.action(KeyContext::Player, dot), Some(Action::NextTrack));
    assert_eq!(
        km.action(KeyContext::SearchResults, dot),
        Some(Action::PageDown)
    );
}

#[test]
fn non_global_rebind_rejects_global_conflict() {
    let mut km = KeyMap::default();
    let help = parse_chord("?").unwrap();

    let err = km
        .rebind(KeyContext::Common, Action::Confirm, help)
        .unwrap_err();

    assert_eq!(err.ctx, KeyContext::Global);
    assert_eq!(err.existing, Action::ToggleHelp);
    assert_eq!(err.chord, help);
}

#[test]
fn global_rebind_rejects_default_context_conflicts() {
    for (ctx, existing, chord) in [
        (
            KeyContext::Global,
            Action::Quit,
            parse_chord("ctrl+q").unwrap(),
        ),
        (
            KeyContext::Player,
            Action::TogglePause,
            parse_chord("space").unwrap(),
        ),
        (
            KeyContext::Common,
            Action::PageUp,
            parse_chord("pageup").unwrap(),
        ),
        (
            KeyContext::Library,
            Action::LibraryFilter,
            parse_chord("/").unwrap(),
        ),
        (
            KeyContext::SearchInput,
            Action::SelectAll,
            parse_chord("ctrl+a").unwrap(),
        ),
    ] {
        let mut km = KeyMap::default();
        let err = km
            .rebind(KeyContext::Global, Action::ToggleHelp, chord)
            .unwrap_err();
        assert_eq!(err.ctx, ctx);
        assert_eq!(err.existing, existing);
        assert_eq!(err.chord, chord);
    }
}

#[test]
fn global_rebind_rejects_dynamically_bound_context_conflicts() {
    for (ctx, action, chord) in [
        (
            KeyContext::Queue,
            Action::QueueRemove,
            parse_chord("f5").unwrap(),
        ),
        (
            KeyContext::SearchResults,
            Action::Enqueue,
            parse_chord("f6").unwrap(),
        ),
        (
            KeyContext::Settings,
            Action::ChangeDecrease,
            parse_chord("f7").unwrap(),
        ),
        (
            KeyContext::AiInput,
            Action::SelectAll,
            parse_chord("f8").unwrap(),
        ),
    ] {
        let mut km = KeyMap::default();
        km.rebind(ctx, action, chord).unwrap();

        let err = km
            .rebind(KeyContext::Global, Action::ToggleHelp, chord)
            .unwrap_err();

        assert_eq!(err.ctx, ctx);
        assert_eq!(err.existing, action);
        assert_eq!(err.chord, chord);
    }
}

#[test]
fn search_source_menu_tab_can_shadow_common_focus_next() {
    let mut km = KeyMap::default();
    let f7 = parse_chord("f7").unwrap();
    let tab = parse_chord("tab").unwrap();

    km.rebind(KeyContext::SearchInput, Action::ToggleSearchSourceMenu, f7)
        .unwrap();
    assert_eq!(
        km.action(KeyContext::SearchInput, f7),
        Some(Action::ToggleSearchSourceMenu)
    );
    assert_eq!(
        km.action(KeyContext::SearchResults, f7),
        Some(Action::ToggleSearchSourceMenu)
    );
    assert_eq!(
        km.action(KeyContext::SearchInput, tab),
        Some(Action::FocusNext)
    );

    km.reset(KeyContext::SearchInput, Action::ToggleSearchSourceMenu)
        .unwrap();
    assert_eq!(
        km.action(KeyContext::SearchInput, tab),
        Some(Action::ToggleSearchSourceMenu)
    );
    assert_eq!(
        km.action(KeyContext::SearchResults, tab),
        Some(Action::ToggleSearchSourceMenu)
    );
    assert!(
        !km.to_overrides()
            .contains_key("search_input.toggle_search_source_menu")
    );
}

#[test]
fn rebinding_search_focus_toggle_updates_both_search_contexts() {
    let mut km = KeyMap::default();
    let f5 = parse_chord("f5").unwrap();
    km.rebind(KeyContext::SearchResults, Action::FocusPrev, f5)
        .unwrap();

    assert_eq!(
        km.context_action(KeyContext::SearchResults, f5),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchInput, f5),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
        None
    );
    assert_eq!(
        km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
        None
    );

    let overrides = km.to_overrides();
    assert_eq!(
        overrides
            .get("search_results.focus_prev")
            .map(String::as_str),
        Some("f5")
    );
    assert_eq!(
        overrides.get("search_input.focus_prev").map(String::as_str),
        Some("f5")
    );
}

#[test]
fn search_focus_toggle_rebind_rejects_conflicts_on_either_side() {
    let mut km = KeyMap::default();
    let select_all = parse_chord("ctrl+a").unwrap();
    let err = km
        .rebind(KeyContext::SearchResults, Action::FocusPrev, select_all)
        .unwrap_err();

    assert_eq!(err.ctx, KeyContext::SearchInput);
    assert_eq!(err.existing, Action::SelectAll);
    assert_eq!(err.chord, select_all);
    assert_eq!(
        km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
}

#[test]
fn overrides_round_trip() {
    let mut km = KeyMap::default();
    km.rebind(
        KeyContext::Player,
        Action::TogglePause,
        parse_chord("f5").unwrap(),
    )
    .unwrap();
    let overrides = km.to_overrides();
    assert_eq!(
        overrides.get("player.toggle_pause").map(String::as_str),
        Some("f5")
    );
    let restored = KeyMap::from_overrides(&overrides);
    assert_eq!(
        restored.action(KeyContext::Player, parse_chord("f5").unwrap()),
        Some(Action::TogglePause)
    );
    assert_eq!(
        restored.action(KeyContext::Player, parse_chord("space").unwrap()),
        None
    );
}

#[test]
fn shuffle_default_is_exposed_on_the_wire() {
    let km = KeyMap::default();
    assert_eq!(
        km.wire_bindings()
            .get("player.toggle_shuffle")
            .map(String::as_str),
        Some("x")
    );
}

#[test]
fn legacy_x_override_keeps_old_shuffle_default() {
    for override_key in ["player.open_library", "common.back", "global.toggle_help"] {
        let mut overrides = BTreeMap::new();
        overrides.insert(override_key.to_owned(), "x".to_owned());
        let km = KeyMap::from_overrides(&overrides);
        assert_eq!(
            km.chord(KeyContext::Player, Action::ToggleShuffle),
            parse_chord("S")
        );
        assert_eq!(
            km.to_overrides()
                .get("player.toggle_shuffle")
                .map(String::as_str),
            Some("S")
        );
    }
}

#[test]
fn explicit_shuffle_override_wins_legacy_migration() {
    for value in ["S", "f8", ""] {
        let mut overrides = BTreeMap::new();
        overrides.insert("player.open_library".to_owned(), "x".to_owned());
        overrides.insert("player.toggle_shuffle".to_owned(), value.to_owned());
        let km = KeyMap::from_overrides(&overrides);
        assert_eq!(
            km.chord(KeyContext::Player, Action::ToggleShuffle),
            parse_chord(value)
        );
    }
}

#[test]
fn legacy_x_and_uppercase_s_claims_leave_shuffle_unbound() {
    let mut overrides = BTreeMap::new();
    overrides.insert("player.open_library".to_owned(), "x".to_owned());
    overrides.insert("global.toggle_help".to_owned(), "S".to_owned());
    let km = KeyMap::from_overrides(&overrides);
    assert_eq!(km.chord(KeyContext::Player, Action::ToggleShuffle), None);
    assert_eq!(
        km.to_overrides()
            .get("player.toggle_shuffle")
            .map(String::as_str),
        Some("")
    );
}

#[test]
fn lyric_delay_bindings_are_editable_and_exposed_on_the_wire() {
    let km = KeyMap::default();
    for (action, id, chord) in [
        (Action::LyricsDelayEarlier, "lyrics_delay_earlier", "z"),
        (Action::LyricsDelayLater, "lyrics_delay_later", "Z"),
    ] {
        assert_eq!(Action::from_id(id), Some(action));
        assert_ne!(action.human_label(), "?");
        assert!(editable_entries().contains(&(KeyContext::Player, action)));
        assert_eq!(km.chord(KeyContext::Player, action), parse_chord(chord));
        assert_eq!(
            km.wire_bindings()
                .get(&format!("player.{id}"))
                .map(String::as_str),
            Some(chord)
        );

        let wire = wire_actions()
            .into_iter()
            .find(|entry| entry.context == "player" && entry.id == id)
            .expect("lyric delay action should be in the wire catalog");
        assert_eq!(wire.default_chord, chord);
        assert_ne!(wire.label, "?");
    }
}

#[test]
fn legacy_z_bindings_win_in_player_common_and_global_contexts() {
    for (override_key, context, existing_action) in [
        (
            "player.open_library",
            KeyContext::Player,
            Action::OpenLibrary,
        ),
        ("common.back", KeyContext::Common, Action::Back),
        ("global.toggle_help", KeyContext::Global, Action::ToggleHelp),
    ] {
        for (chord, delay_action, other_delay, other_chord) in [
            (
                "z",
                Action::LyricsDelayEarlier,
                Action::LyricsDelayLater,
                "Z",
            ),
            (
                "Z",
                Action::LyricsDelayLater,
                Action::LyricsDelayEarlier,
                "z",
            ),
        ] {
            let mut overrides = BTreeMap::new();
            overrides.insert(override_key.to_owned(), chord.to_owned());
            let km = KeyMap::from_overrides(&overrides);
            let claimed = match context {
                KeyContext::Global => km.global_action(parse_chord(chord).unwrap()),
                _ => km.context_action(context, parse_chord(chord).unwrap()),
            };

            assert_eq!(claimed, Some(existing_action), "{override_key} on {chord}");
            assert_eq!(km.chord(KeyContext::Player, delay_action), None);
            assert_eq!(
                km.chord(KeyContext::Player, other_delay),
                parse_chord(other_chord)
            );

            let saved = km.to_overrides();
            assert_eq!(
                saved
                    .get(&format!("player.{}", delay_action.id()))
                    .map(String::as_str),
                Some("")
            );
            assert_eq!(saved.get(override_key).map(String::as_str), Some(chord));
            let restored = KeyMap::from_overrides(&saved);
            assert_eq!(restored.chord(KeyContext::Player, delay_action), None);
        }
    }
}

#[test]
fn explicit_lyric_delay_override_bypasses_legacy_default_migration() {
    let mut overrides = BTreeMap::new();
    overrides.insert("player.open_library".to_owned(), "z".to_owned());
    overrides.insert("player.lyrics_delay_earlier".to_owned(), "f8".to_owned());
    let km = KeyMap::from_overrides(&overrides);

    assert_eq!(
        km.context_action(KeyContext::Player, parse_chord("z").unwrap()),
        Some(Action::OpenLibrary)
    );
    assert_eq!(
        km.context_action(KeyContext::Player, parse_chord("f8").unwrap()),
        Some(Action::LyricsDelayEarlier)
    );
    assert_eq!(
        km.chord(KeyContext::Player, Action::LyricsDelayLater),
        parse_chord("Z")
    );

    let saved = km.to_overrides();
    assert_eq!(
        saved.get("player.lyrics_delay_earlier").map(String::as_str),
        Some("f8")
    );
    let restored = KeyMap::from_overrides(&saved);
    assert_eq!(
        restored.context_action(KeyContext::Player, parse_chord("f8").unwrap()),
        Some(Action::LyricsDelayEarlier)
    );
}

#[test]
fn lyric_delay_remaps_save_and_restore() {
    let mut km = KeyMap::default();
    km.rebind(
        KeyContext::Player,
        Action::LyricsDelayEarlier,
        parse_chord("f8").unwrap(),
    )
    .unwrap();
    km.rebind(
        KeyContext::Player,
        Action::LyricsDelayLater,
        parse_chord("f9").unwrap(),
    )
    .unwrap();

    let restored = KeyMap::from_overrides(&km.to_overrides());
    assert_eq!(
        restored.context_action(KeyContext::Player, parse_chord("f8").unwrap()),
        Some(Action::LyricsDelayEarlier)
    );
    assert_eq!(
        restored.context_action(KeyContext::Player, parse_chord("f9").unwrap()),
        Some(Action::LyricsDelayLater)
    );
    assert_eq!(
        restored.context_action(KeyContext::Player, parse_chord("z").unwrap()),
        None
    );
    assert_eq!(
        restored.context_action(KeyContext::Player, parse_chord("Z").unwrap()),
        None
    );
}

#[test]
fn legacy_open_search_override_still_focuses_search_input() {
    let mut o = BTreeMap::new();
    o.insert("player.open_search".to_owned(), "E".to_owned());
    let km = KeyMap::from_overrides(&o);

    assert_eq!(
        km.action(KeyContext::Player, parse_chord("E").unwrap()),
        Some(Action::OpenSearch)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("E").unwrap()),
        Some(Action::FocusInput)
    );
}

#[test]
fn legacy_search_results_focus_input_override_is_respected() {
    let mut o = BTreeMap::new();
    o.insert("search_results.focus_input".to_owned(), "I".to_owned());
    let km = KeyMap::from_overrides(&o);

    assert_eq!(
        km.action(KeyContext::SearchResults, parse_chord("I").unwrap()),
        Some(Action::FocusInput)
    );
    assert_eq!(
        km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
        Some(Action::FocusPrev)
    );
}

#[test]
fn legacy_global_radio_mode_override_moves_to_player() {
    let mut o = BTreeMap::new();
    o.insert("global.toggle_radio_mode".to_owned(), "f8".to_owned());
    let km = KeyMap::from_overrides(&o);

    assert_eq!(
        km.action(KeyContext::Player, parse_chord("f8").unwrap()),
        Some(Action::ToggleRadioMode)
    );
    assert_eq!(km.global_action(parse_chord("f8").unwrap()), None);
    assert_eq!(
        km.action(KeyContext::Library, parse_chord("f8").unwrap()),
        None
    );
}

#[test]
fn unknown_overrides_are_ignored() {
    let mut o = BTreeMap::new();
    o.insert("bogus.thing".to_owned(), "x".to_owned());
    o.insert(
        "player.toggle_pause".to_owned(),
        "not a real chord!!".to_owned(),
    );
    // Falls back to defaults; doesn't panic.
    let km = KeyMap::from_overrides(&o);
    assert_eq!(
        km.action(KeyContext::Player, parse_chord("space").unwrap()),
        Some(Action::TogglePause)
    );
}

#[test]
fn text_zoom_defaults_bind_ctrl_equals_and_minus_globally() {
    let km = KeyMap::default();
    assert_eq!(
        km.global_action(parse_chord("ctrl+=").unwrap()),
        Some(Action::TextZoomIn)
    );
    assert_eq!(
        km.global_action(parse_chord("ctrl+-").unwrap()),
        Some(Action::TextZoomOut)
    );
    assert_eq!(
        km.global_action(parse_chord("ctrl+l").unwrap()),
        Some(Action::ToggleZoomWheelLock)
    );
    // Ctrl chords are non-typeable, so the zoom keys keep working inside the search
    // and DJ Gem text fields (`is_typeable` gates global suppression there).
    assert!(!parse_chord("ctrl+=").unwrap().is_typeable());
    assert!(!parse_chord("ctrl+-").unwrap().is_typeable());
    // And they survive a config round-trip like any rebindable chord.
    for spelled in ["ctrl+=", "ctrl+-"] {
        let chord = parse_chord(spelled).unwrap();
        assert_eq!(chord_to_config(chord), spelled);
    }
}

#[test]
fn local_deck_accept_all_shadows_global_animation_toggle_on_a() {
    let km = KeyMap::default();
    let chord = parse_chord("A").unwrap();
    assert_eq!(km.global_action(chord), Some(Action::ToggleAnimations));
    assert_eq!(
        km.action(KeyContext::LocalDeck, chord),
        Some(Action::AcceptAllImportReview)
    );
}

#[test]
fn editable_entries_cover_every_binding() {
    assert_eq!(editable_entries().len(), default_bindings().len());
    assert!(
        editable_entries().contains(&(KeyContext::Player, Action::QueueRemove)),
        "Settings > Keys should list the player delete binding"
    );
    assert!(
        editable_entries().contains(&(KeyContext::Player, Action::ToggleRadioMode)),
        "Settings > Keys should list the Radio / Normal mode binding"
    );
    assert!(
        editable_entries().contains(&(KeyContext::Library, Action::ToggleLocalMode)),
        "Settings > Keys should list the Local Deck mode binding"
    );
    assert!(
        editable_entries().contains(&(KeyContext::LocalDeck, Action::AcceptAllImportReview)),
        "Settings > Keys should list the Local Deck accept-all binding"
    );
    // Every action has a stable id and label.
    for (_, action, _) in default_bindings() {
        assert_ne!(action.id(), "?");
        assert_ne!(action.human_label(), "?");
    }
}
