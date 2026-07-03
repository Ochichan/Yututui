//! The persistent protocol-v8 session driver (docs/gui/03 §3.2).
//!
//! One long-lived thread runs a current-thread tokio runtime holding the socket: Hello →
//! subscribe → read frames, with a periodic ping keep-alive and exponential-backoff
//! reconnect. Connection-state transitions are pushed to the event loop via the `emit`
//! callback (the platform code wraps them in its `UserEvent`). Daemon spawn/stop never run
//! here — that would freeze the socket ([01 §9]); the gateway only observes and reports.
//!
//! M0 scope: connect + Hello + live connection state + reconnect. Command/subscription
//! forwarding and topic push fan-out land at M1 (the `player`/`queue` stores).

use std::io;
use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::remote::endpoint;
use crate::remote::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, InstanceFile, InstanceMode,
    PROTOCOL_VERSION, PushEvent, ServerFrame, Topic,
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
}

/// Handle to the gateway thread; dropping it (or calling [`GatewayHandle::stop`]) tears the
/// session down.
pub struct GatewayHandle {
    shutdown: Option<oneshot::Sender<()>>,
}

impl GatewayHandle {
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
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
        rt.block_on(run(emit, shutdown_rx));
    }) {
        tracing::warn!(target: "ytt_desktop", error = %e, "could not start gateway thread");
    }
    GatewayHandle {
        shutdown: Some(shutdown_tx),
    }
}

async fn run<F: Fn(GatewayEvent)>(emit: F, mut shutdown_rx: oneshot::Receiver<()>) {
    let mut backoff = BACKOFF_MIN;
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
                let reason = run_session(conn, &mut shutdown_rx).await;
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
async fn run_session(conn: Stream, shutdown_rx: &mut oneshot::Receiver<()>) -> String {
    let mut reader = BufReader::new(&conn);
    let mut next_id = 1u64;

    // Subscribe to `system` so we notice owner shutdown, and to keep the session non-idle.
    let sub = ClientFrame {
        id: next_id,
        op: ClientOp::Subscribe {
            topics: vec![Topic::System],
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
            line = read_line(&mut reader) => match line {
                Ok(Some(l)) => match serde_json::from_str::<ServerFrame>(l.trim()) {
                    Ok(ServerFrame::Pong { .. }) => awaiting_pong = false,
                    Ok(ServerFrame::Goodbye { reason }) => return reason,
                    Ok(ServerFrame::Event { event: PushEvent::ShuttingDown, .. }) => {
                        return "shutting_down".to_string();
                    }
                    // M0 ignores snapshots/replies; M1 fans them out to the stores.
                    Ok(_) | Err(_) => {}
                },
                Ok(None) | Err(_) => return "disconnected".to_string(),
            }
        }
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
}
