//! Internal library crate for the `ytt` binary and the optional `yututray` helper.
//!
//! The whole module tree is `pub` so the binaries and the unit-test harness can reach
//! it. It is NOT a stable public API: no semver guarantees apply to anything in here —
//! this crate ships binaries, not a library surface.

pub mod ai;
pub mod api;
pub mod app;
pub mod artwork;
pub mod auth_cli;
pub mod cli_capability;
pub mod config;
pub mod daemon;
pub mod data_export;
pub mod deps;
pub mod doctor;
pub mod download;
pub mod downloads;
pub mod eq;
pub mod event;
pub mod i18n;
pub mod ids;
pub mod keymap;
pub mod library;
pub mod local;
pub mod logging;
pub mod lyrics;
pub mod media;
pub mod mousemap;
pub mod notify;
pub(crate) mod owner_event_policy;
pub mod paths;
pub mod persist;
pub mod playback_policy;
pub mod playback_target;
pub mod player;
pub mod playlists;
pub mod queue;
pub mod recorder;
pub mod remote;
pub mod resolver;
pub mod romanize;
pub mod runtime;
pub mod scrobble;
pub mod search_source;
pub mod second_launch;
pub mod session;
pub mod settings;
pub mod signals;
pub mod spotify;
pub mod station;
pub mod streaming;
pub mod terminal_keyboard;
pub mod terminal_runtime;
pub mod theme;
pub mod tools;
pub mod transfer;
pub mod tui;
pub mod ui;
pub mod update;
pub mod util;
pub mod video_overlay;
pub mod why_gem;
pub mod zoom;

#[cfg(test)]
pub mod test_util;

#[cfg(feature = "desktop")]
pub mod desktop;

/// Back-compat shim: `src/tray/` became `src/desktop/` when the tray grew into the full
/// desktop shell (docs/gui/03 §2). Keep the old `crate::tray` path resolving for one
/// release cycle so any external references (and the pre-rename bin) keep building.
#[cfg(feature = "desktop")]
pub mod tray {
    pub use crate::desktop::*;
}
