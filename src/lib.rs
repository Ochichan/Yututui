#![allow(dead_code, unused_imports)]
//! Internal library crate for the `ytt` binary and the optional `ytt-tray` helper.
//!
//! The whole module tree is `pub` so the binaries and the unit-test harness can reach
//! it. It is NOT a stable public API: no semver guarantees apply to anything in here —
//! this crate ships binaries, not a library surface.

pub mod ai;
pub mod api;
pub mod app;
pub mod artwork;
pub mod auth_cli;
pub mod config;
pub mod daemon;
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
pub mod logging;
pub mod lyrics;
pub mod media;
pub mod persist;
pub mod player;
pub mod playlists;
pub mod queue;
pub mod remote;
pub mod resolver;
pub mod romanize;
pub mod runtime;
pub mod scrobble;
pub mod search_source;
pub mod session;
pub mod settings;
pub mod signals;
pub mod spotify;
pub mod station;
pub mod streaming;
pub mod theme;
pub mod transfer;
pub mod tui;
pub mod ui;
pub mod util;

#[cfg(feature = "desktop-tray")]
pub mod tray;
