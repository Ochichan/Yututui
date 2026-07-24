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
pub(crate) mod requests;
pub mod server;
mod sessions;
mod settlement;
pub mod watch;

pub(crate) use sessions::MAX_SESSIONS;
pub use sessions::{RemoteSessionHub, RemoteSessionRef, RemoteSessionScope};
#[cfg(test)]
pub(crate) use sessions::{
    SessionLine, SessionTuning, SubscribeIngress, test_command_reply, test_register,
};

pub use server::{BindOutcome, RemoteReply, RemoteServer, await_primary_release, bind_or_detect};
pub(crate) use settlement::{WireSettlement, WireSettlements};

const QUICK_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const PLAYBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const PERSONAL_EXPORT_REPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_millis(50);

/// Advertised by owners that can build and atomically write a portable personal-data export.
/// Clients must gate the additive command on this capability so a new CLI never sends an
/// unknown command to an older running instance.
pub const PERSONAL_EXPORT_CAPABILITY: &str = "personal-export-v1";
/// Advertised when the export command understands `schema: 2` and returns a v2 causal ledger.
pub const PERSONAL_STATE_V2_CAPABILITY: &str = "personal-state-v2";

/// Daemon/demo capability for the managed long-form seek preference and truthful runtime status.
/// The standalone TUI owner intentionally does not advertise daemon-only GUI mutation support.
pub const LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY: &str = "long-form-seek-optimization-v1";

/// An owner reply first crosses an internal oneshot (or a bounded session queue), then a socket
/// task serializes and flushes it. Hub shutdown cancels those tasks, so hosts which settled
/// pre-close requests wait this bounded handoff window before latching the hub.
pub(crate) async fn await_shutdown_reply_grace() {
    tokio::time::sleep(SHUTDOWN_REPLY_GRACE).await;
}

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
        | RemoteCommand::QueuePlayIfRevision { .. }
        | RemoteCommand::QueueRemoveIfRevision { .. }
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
            schema: None,
        };
        assert_eq!(reply_timeout_for(&command), Duration::from_secs(5 * 60));
    }
}
