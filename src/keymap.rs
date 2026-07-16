//! Central keybinding map: the single source of truth for which key triggers which
//! semantic [`Action`], per input [`KeyContext`].
//!
//! Key handling used to be inline `match k.code` literals scattered across the five
//! `on_key_*` methods, and the on-screen hints were hand-synced string constants. This
//! module decouples *intent* (`Action`) from the physical key ([`Chord`]): handlers
//! resolve an `Action` for their context and act on it, while footers and the `?`
//! cheat-sheet render the bound chords back out — so hints can never drift from behavior.
//!
//! Bindings are user-remappable (the Settings → Keys tab) and persisted to `config.json`
//! as `"<context>.<action>" -> "<chord>"`, storing only entries that differ from the
//! built-in defaults so old configs and future new actions keep working.

mod chord;
mod compat;
mod defaults;
mod display;
mod map;
mod metadata;

pub use chord::{Chord, chord_to_config, chord_to_mpv_input, mpv_overlay_fixed_alias, parse_chord};
pub use defaults::{WireAction, default_bindings, editable_entries, groups, wire_actions};
pub use display::{format_chord, format_chord_for_display, format_chord_retro};
pub use map::{Conflict, KeyMap};
pub use metadata::{Action, KeyContext};

#[cfg(test)]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MediaKeyCode, ModifierKeyCode};
#[cfg(test)]
use std::collections::{BTreeMap, HashMap};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod text_edit_tests;
