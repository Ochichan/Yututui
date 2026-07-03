//! Desktop companion foundation shared by the future macOS, Windows, and Linux tray backends.
//!
//! This module deliberately contains only OS-neutral plumbing: remote-control access, menu state,
//! status polling, and terminal launch planning. Native icon/menu implementations should sit on top
//! of this layer so the terminal player stays small and the platform code remains isolated.

pub mod assets;
pub mod bridge;
pub mod control;
pub mod gateway;
pub mod launch;
pub mod menu_model;
pub mod panel;
pub mod single_instance;
pub mod startup;
pub mod status;
pub mod window_state;

pub mod platform;
