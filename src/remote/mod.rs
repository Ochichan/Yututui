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

pub(crate) use sessions::MAX_SESSIONS;
pub use sessions::{RemoteSessionHub, RemoteSessionRef, RemoteSessionScope};
#[cfg(test)]
pub(crate) use sessions::{SessionLine, SessionTuning, SubscribeIngress, test_register};

pub use server::{BindOutcome, RemoteServer, bind_or_detect};
pub(crate) use settlement::{WireSettlement, WireSettlements};

const QUICK_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const PLAYBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_millis(50);

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
        | RemoteCommand::ResumeSession
        | RemoteCommand::PlayTracks { .. }
        | RemoteCommand::EnqueueTracks { .. } => PLAYBACK_REPLY_TIMEOUT,
        _ => QUICK_REPLY_TIMEOUT,
    }
}
