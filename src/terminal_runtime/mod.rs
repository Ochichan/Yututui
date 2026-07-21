//! Terminal-runtime support for the `ytt` binaries.
//!
//! The binary entrypoint stays thin (`src/main.rs`, also included by `ytt-dev`) while this
//! module owns startup tracing, terminal image probing, and the TUI owner loop.

mod art;
mod persistence_shutdown;
mod persistent_startup;
mod runner;
mod signals;
mod startup;

pub use crate::terminal_policy::STARTUP_OUTPUT_TIMEOUT;
pub use art::{
    build_art_picker, build_art_picker_with_access, build_art_picker_with_access_until,
    build_art_picker_with_access_until_bounded,
};
pub use persistent_startup::{
    PersistentStartupState, TerminalStartupState, load_persistent_startup_state,
};
pub use runner::run;
pub use signals::InteractiveSignals;
pub use startup::StartupTrace;
