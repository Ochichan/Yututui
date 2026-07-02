#![allow(dead_code, unused_imports)]

pub mod ai {
    #[path = "model.rs"]
    pub mod model;
    pub use model::GeminiModel;
}

mod api;
mod config;
pub mod daemon;
mod eq;
pub mod i18n;
#[cfg(test)]
mod keymap;
mod library;
pub mod logging;
pub mod media;
mod player;
mod queue;
pub mod remote;
mod search_source;
mod session;
mod signals;
mod station;
mod streaming;
mod theme;
pub mod util;

#[cfg(feature = "desktop-tray")]
pub mod tray;
