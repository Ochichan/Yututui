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
