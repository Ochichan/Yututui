//! Terminal-runtime support for the `ytt` binaries.
//!
//! The binary entrypoint stays thin (`src/main.rs`, also included by `ytt-dev`) while this
//! module owns startup tracing, terminal image probing, and the TUI owner loop.

mod art;
mod persistence_shutdown;
mod persistent_startup;
mod runner;
mod startup;

pub use art::{build_art_picker, build_art_picker_with_access};
pub use persistent_startup::{
    PersistentStartupState, TerminalStartupState, load_persistent_startup_state,
};
pub use runner::run;
pub use startup::StartupTrace;
