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

use std::time::Duration;

pub mod args;
pub mod client;
pub mod endpoint;
pub mod proto;
pub mod publish;
pub mod server;
mod sessions;

pub use sessions::{RemoteSessionHub, RemoteSessionRef};

pub use server::{BindOutcome, RemoteServer, bind_or_detect};

const QUICK_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const PLAYBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const PERSONAL_EXPORT_REPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Advertised by owners that can build and atomically write a portable personal-data export.
/// Clients must gate the additive command on this capability so a new CLI never sends an
/// unknown command to an older running instance.
pub const PERSONAL_EXPORT_CAPABILITY: &str = "personal-export-v1";

pub(crate) fn reply_timeout_for(command: &proto::RemoteCommand) -> Duration {
    use proto::RemoteCommand;

    match command {
        RemoteCommand::Next
        | RemoteCommand::Prev
        | RemoteCommand::TogglePause
        | RemoteCommand::Play { .. }
        | RemoteCommand::Enqueue { .. }
        | RemoteCommand::QueuePlay { .. }
        | RemoteCommand::QueueRemove { .. }
        | RemoteCommand::ResumeSession
        | RemoteCommand::PlayTracks { .. }
        | RemoteCommand::EnqueueTracks { .. } => PLAYBACK_REPLY_TIMEOUT,
        RemoteCommand::ExportPersonalData { .. } => PERSONAL_EXPORT_REPLY_TIMEOUT,
        _ => QUICK_REPLY_TIMEOUT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_export_has_a_long_running_reply_window() {
        let command = proto::RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        assert_eq!(reply_timeout_for(&command), Duration::from_secs(5 * 60));
    }
}
