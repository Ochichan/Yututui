//! Remote control: `ytt -r <command>` drives an already-running instance over a per-user
//! local socket, and that same socket doubles as a single-instance guard.
//!
//! - [`proto`]    — newline-delimited JSON wire types + the on-disk instance descriptor.
//! - [`args`]     — the `ytt -r` argument parser (verb aliases + display flags).
//! - [`endpoint`] — per-user socket / descriptor paths + token minting.
//! - [`server`]   — single-instance bind + the accept loop (run by the TUI).
//! - [`client`]   — the short-lived `ytt -r` process (never touches the terminal).
//!
//! Commands route through the same reducer path as a keypress (`Msg::Remote` →
//! `App::apply_remote` → `on_player_action`), so they are independent of the TUI's current
//! input mode: `ytt -r next` skips a track even while the UI is in Search text entry.

pub mod args;
pub mod client;
pub mod endpoint;
pub mod proto;
pub mod server;

pub use server::{BindOutcome, RemoteServer, bind_or_detect};
