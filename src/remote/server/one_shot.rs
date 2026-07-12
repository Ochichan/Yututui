//! One-shot remote request validation, owner round-trip, and wire response lifecycle.

use std::io;
use std::time::Duration;

use interprocess::local_socket::tokio::Stream;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::{EventSink, RemoteEvent};
use crate::remote::proto::{
    CONFIRMATION_LOST_REASON, PROTOCOL_VERSION, PROTOCOL_VERSION_V7, RemoteCommand, RemoteRequest,
    RemoteResponse, RemoteResponseEnvelope,
};
use crate::remote::requests::RequestKey;
use crate::remote::{RemoteSessionHub, WireSettlement};

pub(super) struct TrackedRemoteResponse {
    response: RemoteResponse,
    retained_replay: bool,
    settlement: Option<WireSettlement>,
}

impl TrackedRemoteResponse {
    pub(super) fn untracked(response: RemoteResponse) -> Self {
        Self {
            response,
            retained_replay: false,
            settlement: None,
        }
    }
}

pub(super) async fn write_tracked_response(
    conn: &Stream,
    tracked: TrackedRemoteResponse,
    _hub: &RemoteSessionHub,
) -> io::Result<()> {
    #[cfg(test)]
    if !_hub.wire_write_delay().is_zero() {
        tokio::time::sleep(_hub.wire_write_delay()).await;
    }
    let TrackedRemoteResponse {
        response,
        retained_replay,
        settlement,
    } = tracked;
    let envelope = RemoteResponseEnvelope {
        response,
        retained_replay,
    };
    let mut writer = conn;
    let result = write_json_line_to(
        &mut writer,
        &envelope,
        crate::remote::RESPONSE_WRITE_TIMEOUT,
    )
    .await;
    // Success means the newline-delimited response was flushed. A write error/timeout also owns
    // the terminal disposition: there is no live wire left for shutdown to preserve.
    drop(settlement);
    result
}

pub(super) async fn write_response(conn: &Stream, resp: &RemoteResponse) -> io::Result<()> {
    let mut writer = conn;
    write_response_to(&mut writer, resp, crate::remote::RESPONSE_WRITE_TIMEOUT).await
}

pub(super) async fn write_response_to<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &RemoteResponse,
    write_timeout: Duration,
) -> io::Result<()> {
    write_json_line_to(writer, resp, write_timeout).await
}

async fn write_json_line_to<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    resp: &T,
    write_timeout: Duration,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(resp).unwrap_or_else(|_| br#"{"ok":false}"#.to_vec());
    bytes.push(b'\n');

    match timeout(write_timeout, async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "remote response write timed out",
        )),
    }
}

/// Validate one request and retain a lifecycle token only when its owner event was accepted.
pub(super) async fn build_tracked_response(
    req: RemoteRequest,
    token: &str,
    emit: &EventSink,
    hub: &RemoteSessionHub,
) -> TrackedRemoteResponse {
    let untracked = |response| TrackedRemoteResponse::untracked(response);
    // Range, not equality: the v7 one-shot shapes are frozen, so a v8 server keeps serving
    // shipped v7 clients (`ytt -r`, the tray) forever (docs/gui/02 §9).
    if !(PROTOCOL_VERSION_V7..=PROTOCOL_VERSION).contains(&req.version) {
        return untracked(RemoteResponse::err("bad_version"));
    }
    if req.token != token {
        return untracked(RemoteResponse::err("bad_token"));
    }
    if let Err(err) = req.command.validate() {
        return untracked(RemoteResponse::err(err.reason()));
    }
    if matches!(req.command, RemoteCommand::RunSearch { .. }) {
        return untracked(RemoteResponse::err("session_required"));
    }
    if !hub.owner_admission_is_open() {
        return untracked(RemoteResponse::err("shutting_down"));
    }

    let reply_timeout = crate::remote::reply_timeout_for(&req.command);
    if let Some(request_id) = req.request_id {
        let Some(request_key) = RequestKey::stable(request_id) else {
            return untracked(RemoteResponse::err("bad_request_id"));
        };
        // Track every pre-quiesce caller, including a pending join or completed-cache replay. The
        // first peer is not a reliable proxy for a retry's distinct socket write lifecycle.
        let Some(settlement) = hub.admit_tracked(std::convert::identity) else {
            return untracked(RemoteResponse::err("shutting_down"));
        };
        let owner_command = req.command.clone();
        let outcome = hub
            .requests
            .execute_with_replay_proof(request_key, &req.command, reply_timeout, |reply| {
                hub.run_if_owner_admission_open(|| emit(RemoteEvent::Command(owner_command, reply)))
                    .unwrap_or(false)
            })
            .await;
        let mut response = outcome.response;
        let mut retained_replay = outcome.retained_replay;
        if !hub.owner_admission_is_open() && response.reason.as_deref() == Some("server_busy") {
            response = RemoteResponse::err("shutting_down");
            // The response is no longer the byte-for-byte retained outcome, so it cannot prove
            // what the original caller's mutation did.
            retained_replay = false;
        }
        return TrackedRemoteResponse {
            response,
            retained_replay,
            settlement: Some(settlement),
        };
    }

    // Frozen v7 clients have no request identity. Preserve their original one-shot behavior;
    // current clients always send an ID and therefore take the retry-safe path above.
    let (reply_tx, reply_rx) = oneshot::channel();
    let Some(settlement) = hub.admit_tracked(std::convert::identity) else {
        return untracked(RemoteResponse::err("shutting_down"));
    };
    let requires_confirmation = req.command.requires_confirmation();
    if !hub
        .run_if_owner_admission_open(|| emit(RemoteEvent::Command(req.command, reply_tx)))
        .unwrap_or(false)
    {
        let response = if hub.owner_admission_is_open() {
            RemoteResponse::err("server_busy")
        } else {
            RemoteResponse::err("shutting_down")
        };
        return TrackedRemoteResponse {
            response,
            retained_replay: false,
            settlement: Some(settlement),
        };
    }
    let response = match timeout(reply_timeout, reply_rx).await {
        Ok(Ok(resp)) => resp,
        _ if requires_confirmation => RemoteResponse::err(CONFIRMATION_LOST_REASON),
        _ => RemoteResponse::err("timeout"),
    };
    TrackedRemoteResponse {
        response,
        retained_replay: false,
        settlement: Some(settlement),
    }
}

#[cfg(all(test, unix))]
pub(super) async fn build_response(
    req: RemoteRequest,
    token: &str,
    emit: &EventSink,
    hub: &RemoteSessionHub,
) -> RemoteResponse {
    build_tracked_response(req, token, emit, hub).await.response
}
