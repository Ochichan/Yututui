use std::collections::{BTreeMap, HashMap};

use super::{Action, Chord, KeyContext, parse_chord};

/// Keep pre-lyrics-sync user bindings deterministic when their chosen chord becomes a new
/// factory default. A config that already mentions the new action is post-migration and its
/// explicit remap (or unbind) takes precedence over this compatibility rule.
pub(super) fn preserve_legacy_lyrics_delay_overrides(
    overrides: &BTreeMap<String, String>,
    labels: &mut HashMap<(KeyContext, Action), Chord>,
) {
    for (action, config_key) in [
        (Action::LyricsDelayEarlier, "player.lyrics_delay_earlier"),
        (Action::LyricsDelayLater, "player.lyrics_delay_later"),
    ] {
        if overrides.contains_key(config_key) {
            continue;
        }
        let Some(chord) = labels.get(&(KeyContext::Player, action)).copied() else {
            continue;
        };
        if overrides
            .iter()
            .any(|(key, value)| legacy_override_claims(key, value, chord))
        {
            labels.remove(&(KeyContext::Player, action));
        }
    }
}

/// Moving the Player shuffle default from `S` to `x` must not make a pre-existing sparse
/// override on `x` nondeterministic. Keep the old default in that case; an explicit shuffle
/// remap (including an unbind) always wins.
pub(super) fn preserve_legacy_shuffle_override(
    overrides: &BTreeMap<String, String>,
    labels: &mut HashMap<(KeyContext, Action), Chord>,
) {
    if overrides.contains_key("player.toggle_shuffle") {
        return;
    }
    let new_default = parse_chord("x").expect("factory chord");
    if !overrides.iter().any(|(key, value)| {
        legacy_player_scope_claims(key, value, new_default, Action::ToggleShuffle)
    }) {
        return;
    }

    let old_default = parse_chord("S").expect("factory chord");
    if overrides.iter().any(|(key, value)| {
        legacy_player_scope_claims(key, value, old_default, Action::ToggleShuffle)
    }) {
        labels.remove(&(KeyContext::Player, Action::ToggleShuffle));
    } else {
        labels.insert((KeyContext::Player, Action::ToggleShuffle), old_default);
    }
}

/// Newly introduced Common text-editing defaults must not steal chords from explicit
/// sparse overrides written by an older version. Leave each new action unbound until the
/// user chooses a chord explicitly; an explicit remap or unbind for that action always wins.
pub(super) fn preserve_legacy_text_edit_overrides(
    overrides: &BTreeMap<String, String>,
    labels: &mut HashMap<(KeyContext, Action), Chord>,
) {
    for action in [
        Action::DeleteWord,
        Action::MoveCursorLeft,
        Action::MoveCursorRight,
        Action::MoveCursorWordLeft,
        Action::MoveCursorWordRight,
    ] {
        let config_key = format!("common.{}", action.id());
        if overrides.contains_key(&config_key) {
            continue;
        }
        let Some(default) = labels.get(&(KeyContext::Common, action)).copied() else {
            continue;
        };
        if overrides
            .iter()
            .any(|(key, value)| legacy_override_claims_text_chord(key, value, default))
        {
            labels.remove(&(KeyContext::Common, action));
        }
    }
}

fn legacy_override_claims_text_chord(key: &str, value: &str, chord: Chord) -> bool {
    let Some((context_id, action_id)) = key.split_once('.') else {
        return false;
    };
    KeyContext::from_id(context_id).is_some()
        && Action::from_id(action_id).is_some()
        && parse_chord(value) == Some(chord)
}

fn legacy_player_scope_claims(key: &str, value: &str, chord: Chord, excluded: Action) -> bool {
    let Some((context_id, action_id)) = key.split_once('.') else {
        return false;
    };
    let Some(context) = KeyContext::from_id(context_id) else {
        return false;
    };
    if !matches!(
        context,
        KeyContext::Player | KeyContext::Common | KeyContext::Global
    ) {
        return false;
    }
    let Some(action) = Action::from_id(action_id) else {
        return false;
    };
    action != excluded && parse_chord(value) == Some(chord)
}

fn legacy_override_claims(key: &str, value: &str, chord: Chord) -> bool {
    let Some((context_id, action_id)) = key.split_once('.') else {
        return false;
    };
    let Some(context) = KeyContext::from_id(context_id) else {
        return false;
    };
    if !matches!(
        context,
        KeyContext::Player | KeyContext::Common | KeyContext::Global
    ) {
        return false;
    }
    let Some(action) = Action::from_id(action_id) else {
        return false;
    };
    if matches!(
        action,
        Action::LyricsDelayEarlier | Action::LyricsDelayLater
    ) {
        return false;
    }
    parse_chord(value) == Some(chord)
}
