use std::collections::BTreeMap;

use super::*;

const TEXT_EDIT_BINDINGS: [(Action, &str, &str); 6] = [
    (Action::DeleteChar, "delete_char", "backspace"),
    (Action::DeleteWord, "delete_word", "ctrl+backspace"),
    (Action::MoveCursorLeft, "move_cursor_left", "left"),
    (Action::MoveCursorRight, "move_cursor_right", "right"),
    (
        Action::MoveCursorWordLeft,
        "move_cursor_word_left",
        "ctrl+left",
    ),
    (
        Action::MoveCursorWordRight,
        "move_cursor_word_right",
        "ctrl+right",
    ),
];

const MIGRATED_TEXT_EDIT_BINDINGS: [(Action, &str); 5] = [
    (Action::DeleteWord, "ctrl+backspace"),
    (Action::MoveCursorLeft, "left"),
    (Action::MoveCursorRight, "right"),
    (Action::MoveCursorWordLeft, "ctrl+left"),
    (Action::MoveCursorWordRight, "ctrl+right"),
];

#[test]
fn text_edit_chords_parse_and_format_round_trip() {
    for chord in ["left", "right", "ctrl+left", "ctrl+right"] {
        let parsed = parse_chord(chord).unwrap();
        assert_eq!(chord_to_config(parsed), chord);
    }
}

#[test]
fn text_edit_defaults_resolve_directly() {
    let km = KeyMap::default();
    for (action, _, chord) in TEXT_EDIT_BINDINGS {
        assert_eq!(
            km.text_edit_action(parse_chord(chord).unwrap()),
            Some(action),
            "{chord}"
        );
    }
}

#[test]
fn text_edit_bindings_remap_unbind_and_round_trip() {
    let mut km = KeyMap::default();
    let f5 = parse_chord("f5").unwrap();
    km.rebind(KeyContext::Common, Action::MoveCursorWordLeft, f5)
        .unwrap();
    km.unbind(KeyContext::Common, Action::MoveCursorRight);

    assert_eq!(km.text_edit_action(f5), Some(Action::MoveCursorWordLeft));
    assert_eq!(km.text_edit_action(parse_chord("ctrl+left").unwrap()), None);
    assert_eq!(km.text_edit_action(parse_chord("right").unwrap()), None);

    let overrides = km.to_overrides();
    assert_eq!(
        overrides
            .get("common.move_cursor_word_left")
            .map(String::as_str),
        Some("f5")
    );
    assert_eq!(
        overrides
            .get("common.move_cursor_right")
            .map(String::as_str),
        Some("")
    );

    let restored = KeyMap::from_overrides(&overrides);
    assert_eq!(
        restored.text_edit_action(f5),
        Some(Action::MoveCursorWordLeft)
    );
    assert_eq!(
        restored.chord(KeyContext::Common, Action::MoveCursorRight),
        None
    );
}

#[test]
fn text_edit_defaults_are_exposed_on_the_wire() {
    let km = KeyMap::default();
    for (action, id, chord) in TEXT_EDIT_BINDINGS {
        assert_eq!(Action::from_id(id), Some(action));
        assert_ne!(action.human_label(), "?");
        assert!(editable_entries().contains(&(KeyContext::Common, action)));
        assert_eq!(
            km.wire_bindings()
                .get(&format!("common.{id}"))
                .map(String::as_str),
            Some(chord)
        );
        let wire = wire_actions()
            .into_iter()
            .find(|entry| entry.context == "common" && entry.id == id)
            .expect("text-edit action should be in the wire catalog");
        assert_eq!(wire.default_chord, chord);
        assert_ne!(wire.label, "?");
    }
}

#[test]
fn legacy_text_edit_chord_claims_are_preserved() {
    for (action, chord) in MIGRATED_TEXT_EDIT_BINDINGS {
        for override_key in [
            "player.open_library",
            "common.back",
            "global.toggle_help",
            "search_input.select_all",
            "ai_input.select_all",
        ] {
            let mut overrides = BTreeMap::new();
            overrides.insert(override_key.to_owned(), chord.to_owned());
            let km = KeyMap::from_overrides(&overrides);
            assert_eq!(
                km.chord(KeyContext::Common, action),
                None,
                "{override_key} on {chord}"
            );
            let saved = km.to_overrides();
            assert_eq!(
                saved
                    .get(&format!("common.{}", action.id()))
                    .map(String::as_str),
                Some("")
            );
            let restored = KeyMap::from_overrides(&saved);
            assert_eq!(restored.chord(KeyContext::Common, action), None);
        }
    }
}

#[test]
fn explicit_text_edit_overrides_win_legacy_migration() {
    for (action, chord) in MIGRATED_TEXT_EDIT_BINDINGS {
        for value in ["f8", ""] {
            let mut overrides = BTreeMap::new();
            overrides.insert("global.toggle_help".to_owned(), chord.to_owned());
            overrides.insert(format!("common.{}", action.id()), value.to_owned());
            let km = KeyMap::from_overrides(&overrides);
            assert_eq!(
                km.chord(KeyContext::Common, action),
                parse_chord(value),
                "{}={value}",
                action.id()
            );
            if value == "f8" {
                assert_eq!(
                    km.text_edit_action(parse_chord("f8").unwrap()),
                    Some(action)
                );
            }
        }
    }
}
