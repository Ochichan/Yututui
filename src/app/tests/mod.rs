//! Reducer/integration tests for `App`.
//! Domain tests live in small sibling modules; shared fixtures and command
//! assertions live in `support`.

use super::*;
use crossterm::event::{KeyEventKind, KeyEventState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod support;
use support::*;
mod ai;
mod animations;
mod art_overlay;
mod context_menu;
mod docked_bar;
mod downloads;
mod dropdowns_popups;
mod keyboard;
mod keymap_conflicts;
mod library_core;
mod library_drag;
mod library_filter;
mod library_queue_actions;
mod local;
mod local_import;
mod local_import_rows_cache;
#[cfg(test)]
mod local_rows_cache;
mod lyrics_art_download;
mod lyrics_sync_controls;
mod mini_tier;
mod mouse_player;
mod multi_select;
mod nav_mouse;
mod now_playing_overlay;
mod playback_auto_advance;
mod playback_settings;
mod player_composition;
mod player_controls;
mod player_intent_preflight;
mod playlist_search;
mod playlists;
mod queue_mutation_admission;
mod radio_mode;
mod radio_sync;
mod ratings_signals;
mod recorder;
mod recorder_capacity;
mod recorder_shutdown;
mod render_hit_targets;
mod scrollbars_layout;
mod search_keys;
mod search_results;
mod selection_navigation;
#[cfg(test)]
mod settings_cache_policy;
mod settings_color_picker;
mod settings_forms;
mod settings_ui;
mod startup_playback;
mod streaming_extend;
mod track_transitions;
mod transport_recovery;
mod update;
mod video;
mod zoom_retro_nav;
