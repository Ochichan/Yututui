use crate::app::App;
use crate::keymap::{Action, KeyContext, format_chord_for_display};
use crate::queue::Repeat;

/// The current track's tri-state rating glyph: 👍 liked, 👎 disliked, 🤔 neither. Language-neutral
/// Unicode — all three are width-2, so the status line never shifts as the state changes.
pub(super) fn rating_glyph(liked: bool, disliked: bool, retro: bool) -> &'static str {
    match (retro, liked, disliked) {
        (false, true, _) => "👍",
        (false, false, true) => "👎",
        (false, false, false) => "🤔",
        (true, true, _) => "+",
        (true, false, true) => "-",
        (true, false, false) => "?",
    }
}

/// Both shuffle states share one display width, so the centered status line stays fixed.
pub(super) fn shuffle_glyph(on: bool, retro: bool) -> &'static str {
    match (retro, on) {
        (false, true) => "🔀",
        (false, false) => "✗ ",
        (true, true) => "v",
        (true, false) => "x",
    }
}

pub(super) fn player_action_key_label(app: &App, action: Action, retro: bool) -> String {
    app.keymap
        .chord(KeyContext::Player, action)
        .map(|chord| format_chord_for_display(chord, retro))
        .unwrap_or_else(|| "—".to_owned())
}

/// Repeat-all, repeat-one, and off use equal-width glyphs within each display mode.
pub(super) fn repeat_glyph(mode: Repeat, retro: bool) -> &'static str {
    match (retro, mode) {
        (false, Repeat::Off) => "✗ ",
        (false, Repeat::All) => "🔁",
        (false, Repeat::One) => "🔂",
        (true, Repeat::Off) => "x",
        (true, Repeat::All) => "∞",
        (true, Repeat::One) => "1",
    }
}

/// The shuffle slot's stand-in on a live radio stream: the `LIVE:` sync verdict.
pub(super) fn live_sync_glyph(synced: Option<bool>, retro: bool) -> &'static str {
    match (retro, synced) {
        (false, Some(true)) => "✓ ",
        (false, Some(false)) => "✗ ",
        (false, None) => "· ",
        (true, Some(true)) => "v",
        (true, Some(false)) => "x",
        (true, None) => "-",
    }
}

/// The repeat slot's stand-in on a live radio stream: the `SYNC:` re-sync action.
pub(super) fn resync_glyph(retro: bool) -> &'static str {
    if retro { "»" } else { "🔄" }
}
