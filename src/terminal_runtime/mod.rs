//! Terminal-runtime support for the `ytt` binaries.
//!
//! The binary entrypoint stays thin (`src/main.rs`, also included by `ytt-dev`) while this
//! module owns startup tracing, terminal image probing, and the TUI owner loop.

mod art;
mod runner;
mod startup;

pub use art::build_art_picker;
pub use runner::run;
pub use startup::StartupTrace;
