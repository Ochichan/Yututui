//! The persistent protocol-v8 session driver (docs/gui/03 §3.2).
//!
//! One long-lived thread runs a current-thread tokio runtime holding the socket: Hello →
//! subscribe → read frames, with a periodic ping keep-alive and exponential-backoff
//! reconnect. Connection-state transitions are pushed to the event loop via the `emit`
//! callback (the platform code wraps them in its `UserEvent`). Daemon spawn/stop never run
//! here — that would freeze the socket ([01 §9]); the gateway only observes and reports.
//!
//! M1 scope: connect + Hello + live connection state + reconnect, plus the command path —
//! the loop forwards webview `cmd`/`req`/`sub`/`unsub` envelopes into the session
//! ([`GatewayHandle::send`]) and the session fans `event`/`reply` server frames back to the
//! window as [`GatewayEvent::Frame`] (rendered via `bridge::receive_script`).

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::desktop::bridge::{InEnvelope, OutEnvelope, OutKind};
use crate::remote::endpoint;
use crate::remote::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, InstanceFile, InstanceMode,
    PROTOCOL_VERSION, PushEvent, RemoteCommand, RemoteResponse, ServerFrame, Topic,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Live connection state, mirrored into the frontend's `connection` store as `conn` frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online {
        protocol_version: u8,
        capabilities: Vec<String>,
        owner_mode: InstanceMode,
    },
    /// `reason` is a machine string (`"no_core"`, `"no_v8"`, `"bad_token"`, `"disconnected"`,
    /// `"shutting_down"`, …); clients key off it, never a display string.
    Offline {
        reason: String,
    },
}

impl ConnState {
    /// The `conn` envelope payload (camelCase, matching the TS `ConnPayload`) that drives the
    /// frontend's connection store (docs/gui/05 §4.1).
    pub fn to_conn_payload(&self) -> serde_json::Value {
        match self {
            ConnState::Connecting => serde_json::json!({ "state": "connecting" }),
            ConnState::Online {
                protocol_version,
                capabilities,
                owner_mode,
            } => serde_json::json!({
                "state": "online",
                "protocolVersion": protocol_version,
                "capabilities": capabilities,
                "ownerMode": owner_mode,
            }),
            ConnState::Offline { reason } => serde_json::json!({
                "state": "offline",
                "reason": reason,
            }),
        }
    }
}

/// What the gateway thread reports to the event loop.
#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Connection(ConnState),
    /// An inbound server frame (topic push or correlated reply) already rendered as the
    /// webview envelope the loop feeds to `bridge::receive_script` (M1 fan-out).
    Frame(InEnvelope),
}

/// Handle to the gateway thread; dropping it (or calling [`GatewayHandle::stop`]) tears the
/// session down.
pub struct GatewayHandle {
    shutdown: Option<oneshot::Sender<()>>,
    commands: mpsc::UnboundedSender<OutEnvelope>,
}

impl GatewayHandle {
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// Forward a webview envelope (`cmd`/`req`/`sub`/`unsub`) to the live session. Queued
    /// commands are dropped when the session next (re)connects, so a send while offline is a
    /// harmless no-op rather than a stale replay.
    pub fn send(&self, env: OutEnvelope) {
        let _ = self.commands.send(env);
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn the gateway thread. `emit` runs on the gateway thread; the platform wrapper must
/// forward to the loop via `EventLoopProxy::send_event` (it is `!Send`-safe to just clone a
/// proxy into the closure).
pub fn spawn<F>(emit: F) -> GatewayHandle
where
    F: Fn(GatewayEvent) + Send + 'static,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let builder = std::thread::Builder::new().name("ytt-desktop-gateway".to_string());
    if let Err(e) = builder.spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            emit(GatewayEvent::Connection(ConnState::Offline {
                reason: "no_runtime".into(),
            }));
            return;
        };
        rt.block_on(run(emit, shutdown_rx, cmd_rx));
    }) {
        tracing::warn!(target: "ytt_desktop", error = %e, "could not start gateway thread");
    }
    GatewayHandle {
        shutdown: Some(shutdown_tx),
        commands: cmd_tx,
    }
}

async fn run<F: Fn(GatewayEvent)>(
    emit: F,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut cmd_rx: mpsc::UnboundedReceiver<OutEnvelope>,
) {
    let mut backoff = BACKOFF_MIN;
    // The union of topics any window asked for, kept ACROSS sessions. Subscriptions are
    // declarations of interest, not one-shot actions: a window that booted while the core
    // was down (or that outlives a core restart) sent its one `sub` into a dead session,
    // so every fresh session must replay the set or that window stays empty forever.
    let mut desired: Vec<Topic> = Vec::new();
    loop {
        emit(GatewayEvent::Connection(ConnState::Connecting));
        match open_session().await {
            Ok((conn, ack)) => {
                backoff = BACKOFF_MIN; // healthy connection resets the backoff
                emit(GatewayEvent::Connection(ConnState::Online {
                    protocol_version: ack.version,
                    capabilities: ack.capabilities,
                    owner_mode: ack.owner_mode,
                }));
                let reason =
                    run_session(conn, &mut shutdown_rx, &mut cmd_rx, &mut desired, &emit).await;
                emit(GatewayEvent::Connection(ConnState::Offline {
                    reason: reason.clone(),
                }));
                if reason == "shutdown" {
                    return;
                }
            }
            Err(reason) => {
                emit(GatewayEvent::Connection(ConnState::Offline { reason }));
            }
        }

        // Wait out the backoff, but wake immediately on shutdown.
        tokio::select! {
            _ = &mut shutdown_rx => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Read the instance descriptor and complete the Hello handshake.
async fn open_session() -> Result<(Stream, HelloAck), String> {
    let Some(instance) = endpoint::read_instance() else {
        return Err("no_core".to_string());
    };
    if instance.protocol_version < PROTOCOL_VERSION {
        // The owner predates v8 sessions; the tray v7 poll path (status.rs) covers this.
        return Err("no_v8".to_string());
    }
    connect_and_hello(instance).await
}

/// Connect to `instance` and perform the Hello exchange. Factored out for testing against an
/// in-process stub server.
async fn connect_and_hello(instance: InstanceFile) -> Result<(Stream, HelloAck), String> {
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        return Err("bad_endpoint".to_string());
    };
    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        _ => return Err("connect_failed".to_string()),
    };

    let hello = HelloRequest {
        version: PROTOCOL_VERSION,
        token: instance.token,
        hello: HelloBody {
            client: "ytt-desktop".to_string(),
            min_version: PROTOCOL_VERSION,
        },
    };
    if write_line(&conn, &hello).await.is_err() {
        return Err("write_failed".to_string());
    }

    let mut reader = BufReader::new(&conn);
    let ack: HelloAck = match timeout(HELLO_TIMEOUT, read_line(&mut reader)).await {
        Ok(Ok(Some(line))) => match serde_json::from_str(line.trim()) {
            Ok(ack) => ack,
            Err(_) => return Err("bad_ack".to_string()),
        },
        _ => return Err("no_ack".to_string()),
    };
    if !ack.ok {
        return Err(ack.reason.unwrap_or_else(|| "rejected".to_string()));
    }
    drop(reader); // release the borrow so the caller owns `conn` outright
    Ok((conn, ack))
}

/// Drive an established session until it closes; returns the machine reason.
async fn run_session<F: Fn(GatewayEvent)>(
    conn: Stream,
    shutdown_rx: &mut oneshot::Receiver<()>,
    cmd_rx: &mut mpsc::UnboundedReceiver<OutEnvelope>,
    desired: &mut Vec<Topic>,
    emit: &F,
) -> String {
    let mut reader = BufReader::new(&conn);
    let mut next_id = 1u64;
    // Session id of a forwarded `req` → the frontend's own request id, so its `reply` can be
    // correlated back. Only `req` frames insert; every reply removes; the map dies with the
    // session, so it cannot leak across reconnects.
    let mut pending: HashMap<u64, u64> = HashMap::new();

    // Commands queued while we were offline are stale on this fresh session — discard them,
    // EXCEPT sub/unsub, which fold into `desired` so the (re)subscribe below replays them.
    while let Ok(env) = cmd_rx.try_recv() {
        track_subscriptions(&env, desired);
    }

    // Subscribe to `system` (so we notice owner shutdown even with no window open, and to
    // keep the session non-idle) plus every topic a window already declared. New sessions
    // send fresh snapshots for all of these, which is exactly the rehydrate the frontend
    // expects after a reconnect.
    let sub = ClientFrame {
        id: next_id,
        op: ClientOp::Subscribe {
            topics: initial_topics(desired),
        },
    };
    next_id += 1;
    if write_line(&conn, &sub).await.is_err() {
        return "disconnected".to_string();
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // consume the immediate first tick
    let mut awaiting_pong = false;

    loop {
        tokio::select! {
            _ = &mut *shutdown_rx => return "shutdown".to_string(),
            _ = ping.tick() => {
                if awaiting_pong {
                    return "ping_timeout".to_string();
                }
                let frame = ClientFrame { id: next_id, op: ClientOp::Ping };
                next_id += 1;
                if write_line(&conn, &frame).await.is_err() {
                    return "disconnected".to_string();
                }
                awaiting_pong = true;
            }
            maybe = cmd_rx.recv() => {
                if let Some(env) = maybe {
                    track_subscriptions(&env, desired);
                    if let Some(reason) =
                        forward_command(&conn, env, &mut next_id, &mut pending, emit).await
                    {
                        return reason;
                    }
                }
                // `None` = the handle was dropped; the shutdown branch will fire too.
            }
            line = read_line(&mut reader) => match line {
                Ok(Some(l)) => match serde_json::from_str::<ServerFrame>(l.trim()) {
                    Ok(ServerFrame::Pong { .. }) => awaiting_pong = false,
                    Ok(ServerFrame::Goodbye { reason }) => return reason,
                    Ok(ServerFrame::Reply { id, resp }) => {
                        if let Some(fid) = pending.remove(&id) {
                            emit(GatewayEvent::Frame(reply_envelope(fid, resp)));
                        }
                        // Unmapped ids are the gateway's own subscribe/ping replies — ignore.
                    }
                    Ok(ServerFrame::Event { topic, event, .. }) => {
                        let payload =
                            serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                        emit(GatewayEvent::Frame(InEnvelope::event(topic.wire_str(), payload)));
                        if matches!(event, PushEvent::ShuttingDown) {
                            return "shutting_down".to_string();
                        }
                    }
                    Err(_) => {}
                },
                Ok(None) | Err(_) => return "disconnected".to_string(),
            }
        }
    }
}

/// Translate a webview envelope into a [`ClientFrame`] and write it to the session. Returns
/// `Some(reason)` only on a socket write failure (which ends the session); a command that
/// can't be translated is rejected (for `req`) or dropped (for `cmd`) without tearing down.
async fn forward_command<F: Fn(GatewayEvent)>(
    conn: &Stream,
    env: OutEnvelope,
    next_id: &mut u64,
    pending: &mut HashMap<u64, u64>,
    emit: &F,
) -> Option<String> {
    let op = match env.kind {
        OutKind::Cmd | OutKind::Req => match to_remote_command(&env.name, &env.payload) {
            Some(cmd) => ClientOp::Command(cmd),
            None => {
                // Unsupported command name/shape. A correlated req must not hang for its 10 s
                // timeout, so reject it; a fire-and-forget cmd is just logged and dropped.
                if let (OutKind::Req, Some(fid)) = (env.kind, env.id) {
                    emit(GatewayEvent::Frame(InEnvelope::err(
                        fid,
                        serde_json::json!({ "reason": "bad_command" }),
                    )));
                } else {
                    tracing::debug!(
                        target: "ytt_desktop",
                        name = %env.name,
                        "dropping unsupported gateway command"
                    );
                }
                return None;
            }
        },
        OutKind::Sub => match parse_topics(&env.payload) {
            Some(topics) => ClientOp::Subscribe { topics },
            None => return None,
        },
        OutKind::Unsub => match parse_topics(&env.payload) {
            Some(topics) => ClientOp::Unsubscribe { topics },
            None => return None,
        },
        // `win` never reaches the gateway (bridge routes it natively); ignore defensively.
        OutKind::Win => return None,
    };

    let sid = *next_id;
    *next_id += 1;
    if let (OutKind::Req, Some(fid)) = (env.kind, env.id) {
        pending.insert(sid, fid);
    }
    let frame = ClientFrame { id: sid, op };
    if write_line(conn, &frame).await.is_err() {
        return Some("disconnected".to_string());
    }
    None
}

/// Build a `{"cmd": name, ...payload}` object and parse it as a [`RemoteCommand`]. The command
/// enum is `#[serde(tag = "cmd")]` snake_case, so the frontend's `name` is the tag and its
/// `payload` supplies the fields. Returns `None` for names/shapes the core doesn't model.
fn to_remote_command(name: &str, payload: &serde_json::Value) -> Option<RemoteCommand> {
    let mut obj = match payload {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::Null => serde_json::Map::new(),
        _ => return None,
    };
    obj.insert(
        "cmd".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    serde_json::from_value(serde_json::Value::Object(obj)).ok()
}

/// A `sub`/`unsub` payload is a JSON array of wire topic strings. Empty is treated as nothing
/// to do (the client already guards this, but be defensive).
fn parse_topics(payload: &serde_json::Value) -> Option<Vec<Topic>> {
    let topics: Vec<Topic> = serde_json::from_value(payload.clone()).ok()?;
    (!topics.is_empty()).then_some(topics)
}

/// Fold a `sub`/`unsub` envelope into the reconnect-surviving desired-topic set; every other
/// kind passes through untouched. Order-preserving and deduplicated (the set stays small —
/// at most the 13 wire topics).
fn track_subscriptions(env: &OutEnvelope, desired: &mut Vec<Topic>) {
    match env.kind {
        OutKind::Sub => {
            if let Some(topics) = parse_topics(&env.payload) {
                for topic in topics {
                    if !desired.contains(&topic) {
                        desired.push(topic);
                    }
                }
            }
        }
        OutKind::Unsub => {
            if let Some(topics) = parse_topics(&env.payload) {
                desired.retain(|topic| !topics.contains(topic));
            }
        }
        _ => {}
    }
}

/// The session-opening subscribe: `system` first (the gateway's own keep-alive/shutdown
/// listener), then every window-declared topic.
fn initial_topics(desired: &[Topic]) -> Vec<Topic> {
    let mut topics = vec![Topic::System];
    for topic in desired {
        if !topics.contains(topic) {
            topics.push(*topic);
        }
    }
    topics
}

/// Render a correlated [`RemoteResponse`] as the `res`/`err` envelope the frontend awaits.
fn reply_envelope(fid: u64, resp: RemoteResponse) -> InEnvelope {
    if resp.ok {
        let payload = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
        InEnvelope::res(fid, payload)
    } else {
        let reason = resp.reason.unwrap_or_else(|| "error".to_string());
        InEnvelope::err(fid, serde_json::json!({ "reason": reason }))
    }
}

async fn write_line<T: serde::Serialize>(conn: &Stream, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    let mut w = conn;
    w.write_all(&buf).await?;
    w.flush().await
}

async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    Ok((n != 0).then_some(line))
}

// Envelope→frame translation is platform-agnostic, so these run on every target (the socket
// tests below need a unix domain path and are gated separately).
#[cfg(test)]
mod translate_tests {
    use super::*;
    use crate::desktop::bridge::InKind;
    use crate::remote::proto::{RemoteCommand, ToggleState};

    #[test]
    fn cmd_names_and_payloads_map_to_remote_commands() {
        assert_eq!(
            to_remote_command("toggle_pause", &serde_json::Value::Null),
            Some(RemoteCommand::TogglePause)
        );
        assert_eq!(
            to_remote_command("next", &serde_json::json!({})),
            Some(RemoteCommand::Next)
        );
        assert_eq!(
            to_remote_command("seek_to", &serde_json::json!({ "ms": 1000 })),
            Some(RemoteCommand::SeekTo { ms: 1000 })
        );
        assert_eq!(
            to_remote_command("set_volume", &serde_json::json!({ "percent": 42 })),
            Some(RemoteCommand::SetVolume { percent: 42 })
        );
        assert_eq!(
            to_remote_command("queue_play", &serde_json::json!({ "position": 3 })),
            Some(RemoteCommand::QueuePlay { position: 3 })
        );
        assert_eq!(
            to_remote_command("streaming", &serde_json::json!({ "state": "on" })),
            Some(RemoteCommand::Streaming {
                state: ToggleState::On
            })
        );
        // The GUI-session verbs the frontend already sends (search + row playback).
        assert_eq!(
            to_remote_command(
                "run_search",
                &serde_json::json!({ "ticket": 3, "query": "queen", "source": "all" })
            ),
            Some(RemoteCommand::RunSearch {
                ticket: 3,
                query: "queen".to_string(),
                source: crate::search_source::SearchSource::All,
            })
        );
        assert_eq!(
            to_remote_command(
                "play_tracks",
                &serde_json::json!({ "video_ids": ["a", "b"] })
            ),
            Some(RemoteCommand::PlayTracks {
                video_ids: vec!["a".to_string(), "b".to_string()],
            })
        );
        assert_eq!(
            to_remote_command("enqueue_tracks", &serde_json::json!({ "video_ids": ["c"] })),
            Some(RemoteCommand::EnqueueTracks {
                video_ids: vec!["c".to_string()],
            })
        );
    }

    #[test]
    fn unsupported_or_malformed_commands_are_none() {
        // A v8 command the core doesn't model yet (queue/rating extensions).
        assert_eq!(
            to_remote_command(
                "rate",
                &serde_json::json!({ "video_id": "x", "rating": "cycle" })
            ),
            None
        );
        assert_eq!(
            to_remote_command("not_a_command", &serde_json::Value::Null),
            None
        );
        // Right name, wrong field type.
        assert_eq!(
            to_remote_command("seek_to", &serde_json::json!({ "ms": "soon" })),
            None
        );
        // Non-object, non-null payloads can't carry command fields.
        assert_eq!(to_remote_command("next", &serde_json::json!(7)), None);
    }

    #[test]
    fn topics_parse_from_the_wire_array() {
        assert_eq!(
            parse_topics(&serde_json::json!(["player", "queue", "system"])),
            Some(vec![Topic::Player, Topic::Queue, Topic::System])
        );
        assert_eq!(parse_topics(&serde_json::json!([])), None);
        assert_eq!(parse_topics(&serde_json::json!("player")), None);
        assert_eq!(parse_topics(&serde_json::json!(["bogus"])), None);
    }

    fn sub_env(kind: OutKind, topics: serde_json::Value) -> OutEnvelope {
        OutEnvelope {
            v: 1,
            id: None,
            kind,
            name: String::new(),
            payload: topics,
        }
    }

    #[test]
    fn subscriptions_fold_into_the_desired_set_across_kinds() {
        let mut desired = Vec::new();
        track_subscriptions(
            &sub_env(OutKind::Sub, serde_json::json!(["player", "queue"])),
            &mut desired,
        );
        // Overlapping re-sub dedups instead of duplicating.
        track_subscriptions(
            &sub_env(OutKind::Sub, serde_json::json!(["queue", "settings"])),
            &mut desired,
        );
        track_subscriptions(
            &sub_env(OutKind::Unsub, serde_json::json!(["queue"])),
            &mut desired,
        );
        assert_eq!(desired, vec![Topic::Player, Topic::Settings]);

        // Non-subscription envelopes never touch the set.
        track_subscriptions(
            &OutEnvelope {
                v: 1,
                id: Some(1),
                kind: OutKind::Cmd,
                name: "next".to_string(),
                payload: serde_json::Value::Null,
            },
            &mut desired,
        );
        assert_eq!(desired, vec![Topic::Player, Topic::Settings]);
    }

    #[test]
    fn initial_subscribe_replays_desired_topics_after_system() {
        assert_eq!(initial_topics(&[]), vec![Topic::System]);
        assert_eq!(
            initial_topics(&[Topic::Player, Topic::Queue]),
            vec![Topic::System, Topic::Player, Topic::Queue]
        );
        // A window that explicitly subscribed to system doesn't double it.
        assert_eq!(
            initial_topics(&[Topic::System, Topic::Player]),
            vec![Topic::System, Topic::Player]
        );
    }

    #[test]
    fn reply_envelope_maps_ok_and_error() {
        let ok = reply_envelope(5, RemoteResponse::ok("done".to_string()));
        assert_eq!(ok.id, Some(5));
        assert_eq!(ok.kind, InKind::Res);

        let bad = reply_envelope(
            6,
            RemoteResponse {
                ok: false,
                reason: Some("bad_request".to_string()),
                message: None,
                status: None,
            },
        );
        assert_eq!(bad.id, Some(6));
        assert_eq!(bad.kind, InKind::Err);
        assert_eq!(
            bad.payload,
            Some(serde_json::json!({ "reason": "bad_request" }))
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::tokio::Listener;

    fn test_instance(endpoint: String, token: &str) -> InstanceFile {
        InstanceFile {
            app_pid: std::process::id(),
            endpoint,
            token: token.to_string(),
            created_unix: 1,
            mode: InstanceMode::Daemon,
            protocol_version: PROTOCOL_VERSION,
            capabilities: vec!["events-v8".to_string()],
        }
    }

    fn bind(endpoint: &str) -> Listener {
        let _ = std::fs::remove_file(endpoint);
        let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
        ListenerOptions::new().name(name).create_tokio().unwrap()
    }

    async fn serve_hello(listener: Listener, ack: HelloAck) {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).await.unwrap();
        // The client greeted us with a v8 Hello carrying the shared token.
        assert!(hello_line.contains("\"hello\""));
        let mut w = &conn;
        let mut buf = serde_json::to_vec(&ack).unwrap();
        buf.push(b'\n');
        w.write_all(&buf).await.unwrap();
        w.flush().await.unwrap();
        // Hold the connection briefly so the client sees an established session.
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            reader.read_line(&mut String::new()),
        )
        .await;
    }

    #[tokio::test]
    async fn hello_handshake_succeeds() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-ok-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let ack = HelloAck {
            ok: true,
            version: 8,
            session_id: 1,
            capabilities: vec!["events-v8".to_string()],
            owner_mode: InstanceMode::Daemon,
            reason: None,
        };
        let server = tokio::spawn(serve_hello(listener, ack));
        let result = connect_and_hello(test_instance(endpoint.clone(), "tok")).await;
        server.abort();
        let _ = std::fs::remove_file(&endpoint);
        let (_conn, ack) = result.expect("handshake should succeed");
        assert_eq!(ack.version, 8);
        assert!(ack.capabilities.contains(&"events-v8".to_string()));
    }

    #[tokio::test]
    async fn hello_rejection_surfaces_the_reason() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-bad-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let ack = HelloAck {
            ok: false,
            version: 8,
            session_id: 0,
            capabilities: vec![],
            owner_mode: InstanceMode::Daemon,
            reason: Some("bad_token".to_string()),
        };
        let server = tokio::spawn(serve_hello(listener, ack));
        let err = connect_and_hello(test_instance(endpoint.clone(), "wrong"))
            .await
            .unwrap_err();
        server.abort();
        let _ = std::fs::remove_file(&endpoint);
        assert_eq!(err, "bad_token");
    }

    #[tokio::test]
    async fn missing_core_is_reported() {
        let err = connect_and_hello(test_instance(
            std::env::temp_dir()
                .join("ytt-gw-nope.sock")
                .to_string_lossy()
                .into_owned(),
            "tok",
        ))
        .await
        .unwrap_err();
        assert_eq!(err, "connect_failed");
    }

    /// Accept one connection and return the first line the client writes.
    async fn accept_one_line(listener: Listener) -> String {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    }

    async fn connect(endpoint: &str) -> Stream {
        let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
        Stream::connect(name).await.unwrap()
    }

    #[tokio::test]
    async fn forward_cmd_translates_and_rewrites_the_id() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-fwd-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let server = tokio::spawn(accept_one_line(listener));
        let conn = connect(&endpoint).await;

        // The frontend's own id (99) must never reach the wire — the session allocates its own.
        let env = OutEnvelope {
            v: 1,
            id: Some(99),
            kind: OutKind::Cmd,
            name: "seek_to".to_string(),
            payload: serde_json::json!({ "ms": 1234 }),
        };
        let mut next_id = 5u64;
        let mut pending = HashMap::new();
        let reason = forward_command(&conn, env, &mut next_id, &mut pending, &|_| {}).await;
        assert!(reason.is_none(), "a good write must not end the session");

        let line = server.await.unwrap();
        let _ = std::fs::remove_file(&endpoint);
        let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            frame.id, 5,
            "wire id is session-allocated, not the frontend's 99"
        );
        assert_eq!(
            frame.op,
            ClientOp::Command(RemoteCommand::SeekTo { ms: 1234 })
        );
        assert_eq!(next_id, 6, "id counter advanced");
        assert!(
            pending.is_empty(),
            "a fire-and-forget cmd is not correlated"
        );
    }

    #[tokio::test]
    async fn forward_req_records_the_reply_correlation() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-req-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let server = tokio::spawn(accept_one_line(listener));
        let conn = connect(&endpoint).await;

        let env = OutEnvelope {
            v: 1,
            id: Some(77),
            kind: OutKind::Req,
            name: "status".to_string(),
            payload: serde_json::Value::Null,
        };
        let mut next_id = 1u64;
        let mut pending = HashMap::new();
        forward_command(&conn, env, &mut next_id, &mut pending, &|_| {}).await;

        let line = server.await.unwrap();
        let _ = std::fs::remove_file(&endpoint);
        let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(frame.id, 1);
        assert_eq!(frame.op, ClientOp::Command(RemoteCommand::Status));
        // Session id 1 maps back to the frontend's request id 77 so its reply can be routed.
        assert_eq!(pending.get(&1), Some(&77));
    }
}
