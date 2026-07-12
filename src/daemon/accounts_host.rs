//! Owner-loop accounts glue (C5): the Last.fm connect flow and scrobble auth events.
//!
//! The scrobble actor is owned by the serve loop (it also feeds playback observation),
//! so this stays a set of free functions the loop calls with the handle — unlike the
//! downloads/AI hosts there is no actor to own here. Secrets flow one way: the session
//! key from `AuthDone` goes into config and never back onto the wire.

use crate::remote::proto::{RemoteCommand, RemoteResponse};
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;
use crate::scrobble::{ScrobbleEvent, ScrobbleHandle};

use super::engine::DaemonEngine;
use super::events::DaemonEvent;

/// Intercept `lastfm_connect` before engine dispatch (the engine has no scrobble
/// handle). Returns the event back when it is not ours.
pub(super) fn intercept(event: DaemonEvent, scrobble: &ScrobbleHandle) -> Option<DaemonEvent> {
    match event {
        DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => {
            match connect_command(command, scrobble) {
                Ok(response) => {
                    let _ = reply.send(response);
                    None
                }
                Err(command) => Some(DaemonEvent::Remote(RemoteEvent::Command(command, reply))),
            }
        }
        DaemonEvent::Remote(RemoteEvent::SessionCommand {
            command,
            origin,
            reply,
        }) => match connect_command(command, scrobble) {
            Ok(response) => {
                let _ = reply.send(response);
                None
            }
            Err(command) => Some(DaemonEvent::Remote(RemoteEvent::SessionCommand {
                command,
                origin,
                reply,
            })),
        },
        event => Some(event),
    }
}

fn connect_command(
    command: RemoteCommand,
    scrobble: &ScrobbleHandle,
) -> Result<RemoteResponse, RemoteCommand> {
    match command {
        RemoteCommand::LastfmConnect => Ok(match scrobble.auth_start() {
            Ok(_) => RemoteResponse::ok("lastfm auth started".to_owned()),
            // The actor is mid-configuration or draining; the GUI simply retries.
            Err(_) => RemoteResponse::err("busy"),
        }),
        command => Err(command),
    }
}

/// Route a scrobble-actor event: the auth trio feeds the `accounts` topic; everything
/// else keeps its historical log-only behavior. Engine account changes surface through
/// `accounts_rev`, which the serve-loop tail publishes and reconciles (with retry).
pub(super) fn on_scrobble_event(
    event: ScrobbleEvent,
    engine: &mut DaemonEngine,
    publisher: &mut Publisher,
) {
    match event {
        ScrobbleEvent::AuthUrl(url) => {
            // One-shot: the GUI opens the browser. Debug-logged (the URL embeds the
            // request token; headless operators can raise the log level to copy it —
            // the TUI never logs it at all).
            tracing::debug!(%url, "lastfm authorization url");
            publisher.publish_accounts_auth_url("lastfm", &url);
        }
        ScrobbleEvent::AuthDone {
            username,
            session_key,
        } => {
            engine.apply_lastfm_session(username, session_key);
        }
        ScrobbleEvent::AuthFailed(error) => {
            let error = crate::util::sanitize::sanitize_error_text(error);
            tracing::warn!(%error, "lastfm authorization failed");
            publisher.publish_accounts_auth_failed("lastfm", &error);
        }
        other => {
            super::log_scrobble_event(other);
        }
    }
}
