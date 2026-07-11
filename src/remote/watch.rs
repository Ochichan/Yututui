//! Read-only `ytt -r watch` client for protocol-v8 push sessions.
//!
//! This deliberately does not reuse the desktop gateway: the CLI opens one authenticated
//! local-socket session, subscribes to the small read-only topic surface, and exits when that
//! connection ends. There is no reconnect or replay path.

use std::future::Future;
use std::io::{self, Write};
use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::{Instant, interval_at, timeout};

use super::endpoint;
use super::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, MAX_ONESHOT_REPLY_BYTES,
    PROTOCOL_VERSION, PushEvent, ServerFrame, Topic,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const MAX_FRAME_BYTES: usize = MAX_ONESHOT_REPLY_BYTES;
const OUTPUT_QUEUE_CAPACITY: usize = 16;
const OUTPUT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;
const SUBSCRIBE_ID: u64 = 1;

/// Run a read-only protocol-v8 watch session against the current, validated owner.
pub async fn run(topics: Vec<crate::remote::proto::Topic>, json: bool, quiet: bool) -> i32 {
    race_operation_with_cancel(run_operation(topics, json, quiet), tokio::signal::ctrl_c()).await
}

/// Poll cancellation first so Tokio installs its signal handler before descriptor I/O,
/// connect, or either handshake write can run. Once installed, SIGINT remains captured even
/// while a synchronous descriptor check is in progress, and every Ctrl-C path exits cleanly.
async fn race_operation_with_cancel<O, C>(operation: O, cancel: C) -> i32
where
    O: Future<Output = i32>,
    C: Future<Output = io::Result<()>>,
{
    tokio::pin!(operation);
    tokio::pin!(cancel);
    tokio::select! {
        biased;
        cancel_result = &mut cancel => match cancel_result {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("ytt -r: could not listen for Ctrl-C: {error}");
                EXIT_TRANSPORT
            }
        },
        exit_code = &mut operation => exit_code,
    }
}

async fn run_operation(topics: Vec<Topic>, json: bool, quiet: bool) -> i32 {
    let topics = match normalize_topics(topics) {
        Ok(topics) => topics,
        Err(error) => {
            eprintln!("ytt -r: {}", error.message);
            return error.exit_code();
        }
    };

    let instance = match endpoint::read_current_instance() {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("ytt -r: {error}");
            return EXIT_TRANSPORT;
        }
    };
    if instance.protocol_version < PROTOCOL_VERSION {
        eprintln!(
            "ytt -r: watch requires protocol {PROTOCOL_VERSION} or newer (owner advertises {}).",
            instance.protocol_version
        );
        return EXIT_USAGE;
    }
    if !has_events_capability(&instance.capabilities) {
        eprintln!("ytt -r: the running ytt instance does not support watch events.");
        return EXIT_USAGE;
    }

    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        eprintln!("ytt -r: malformed endpoint in the instance descriptor.");
        return EXIT_TRANSPORT;
    };
    let stream = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            eprintln!("ytt -r: could not reach ytt (it may have exited).");
            return EXIT_TRANSPORT;
        }
    };

    let config = SessionConfig {
        token: instance.token,
        topics,
        json,
        quiet,
        io_timeout: IO_TIMEOUT,
        ping_interval: PING_INTERVAL,
    };
    match run_session_with_output(
        stream,
        config,
        std::future::pending::<Result<(), WatchError>>(),
        io::stdout(),
        OUTPUT_QUEUE_CAPACITY,
        OUTPUT_FLUSH_TIMEOUT,
    )
    .await
    {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("ytt -r: {}", error.message);
            error.exit_code()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    Transport,
    Unsupported,
}

#[derive(Debug, PartialEq, Eq)]
struct WatchError {
    kind: ErrorKind,
    message: String,
}

impl WatchError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Transport,
            message: message.into(),
        }
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            message: message.into(),
        }
    }

    fn exit_code(&self) -> i32 {
        match self.kind {
            ErrorKind::Transport => EXIT_TRANSPORT,
            ErrorKind::Unsupported => EXIT_USAGE,
        }
    }
}

#[derive(Debug)]
struct SessionConfig {
    token: String,
    topics: Vec<Topic>,
    json: bool,
    quiet: bool,
    io_timeout: Duration,
    ping_interval: Duration,
}

fn normalize_topics(topics: Vec<Topic>) -> Result<Vec<Topic>, WatchError> {
    if topics.is_empty() {
        return Err(WatchError::unsupported(
            "watch requires at least one topic.",
        ));
    }

    let mut normalized = Vec::with_capacity(topics.len());
    for topic in topics {
        if !matches!(
            topic,
            Topic::Player | Topic::Queue | Topic::Settings | Topic::System
        ) {
            return Err(WatchError::unsupported(format!(
                "unsupported watch topic `{}`.",
                topic.wire_str()
            )));
        }
        if !normalized.contains(&topic) {
            normalized.push(topic);
        }
    }
    Ok(normalized)
}

fn has_events_capability(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|capability| capability == "events-v8")
}

fn spawn_output_writer<W>(
    mut writer: W,
    capacity: usize,
) -> Result<
    (
        std::sync::mpsc::SyncSender<String>,
        tokio::sync::oneshot::Receiver<io::Result<()>>,
    ),
    WatchError,
>
where
    W: Write + Send + 'static,
{
    let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<String>(capacity);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("ytt-watch-output".to_string())
        .spawn(move || {
            let result = (|| {
                while let Ok(mut line) = line_rx.recv() {
                    line.push('\n');
                    writer.write_all(line.as_bytes())?;
                    writer.flush()?;
                }
                writer.flush()
            })();
            let _ = done_tx.send(result);
        })
        .map_err(|_| WatchError::transport("could not start the watch output writer."))?;
    Ok((line_tx, done_rx))
}

fn output_completion(
    result: Result<io::Result<()>, tokio::sync::oneshot::error::RecvError>,
) -> Result<(), WatchError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(WatchError::transport("watch output failed.")),
        Err(_) => Err(WatchError::transport(
            "watch output writer stopped unexpectedly.",
        )),
    }
}

fn enqueue_output_line(
    line_tx: &std::sync::mpsc::SyncSender<String>,
    line: &str,
) -> io::Result<()> {
    line_tx.try_send(line.to_string()).map_err(|error| {
        let (kind, message) = match error {
            std::sync::mpsc::TrySendError::Full(_) => (
                io::ErrorKind::WouldBlock,
                "watch output consumer is too slow.",
            ),
            std::sync::mpsc::TrySendError::Disconnected(_) => {
                (io::ErrorKind::BrokenPipe, "watch output writer stopped.")
            }
        };
        io::Error::new(kind, message)
    })
}

async fn run_session_with_output<S, C, W>(
    stream: S,
    config: SessionConfig,
    cancel: C,
    writer: W,
    output_capacity: usize,
    output_timeout: Duration,
) -> Result<(), WatchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Future<Output = Result<(), WatchError>>,
    W: Write + Send + 'static,
{
    let (line_tx, mut output_done) = spawn_output_writer(writer, output_capacity)?;
    let session_result = {
        let mut emit = move |line: &str| enqueue_output_line(&line_tx, line);
        let session = run_session(stream, config, cancel, &mut emit);
        tokio::pin!(session);
        tokio::select! {
            biased;
            writer_result = &mut output_done => {
                output_completion(writer_result)?;
                return Err(WatchError::transport(
                    "watch output writer stopped unexpectedly.",
                ));
            }
            result = &mut session => result,
        }
    };

    let writer_result = timeout(output_timeout, &mut output_done)
        .await
        .map_err(|_| WatchError::transport("timed out flushing watch output."))?;
    output_completion(writer_result)?;
    session_result
}

async fn run_session<S, C, E>(
    stream: S,
    config: SessionConfig,
    cancel: C,
    emit: &mut E,
) -> Result<(), WatchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Future<Output = Result<(), WatchError>>,
    E: FnMut(&str) -> io::Result<()>,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let hello = HelloRequest {
        version: PROTOCOL_VERSION,
        token: config.token,
        hello: HelloBody {
            client: "ytt-cli-watch".to_string(),
            min_version: PROTOCOL_VERSION,
        },
    };
    write_json_line(&mut write_half, &hello, config.io_timeout, "Hello").await?;

    let ack: HelloAck = timeout(
        config.io_timeout,
        read_json_line(&mut reader, MAX_FRAME_BYTES),
    )
    .await
    .map_err(|_| WatchError::transport("timed out waiting for the watch handshake."))??;
    validate_ack(&ack)?;

    let subscribe = ClientFrame {
        id: SUBSCRIBE_ID,
        op: ClientOp::Subscribe {
            topics: config.topics.clone(),
        },
    };
    write_json_line(
        &mut write_half,
        &subscribe,
        config.io_timeout,
        "subscription",
    )
    .await?;

    let first_tick = Instant::now() + config.ping_interval;
    let mut ping_timer = interval_at(first_tick, config.ping_interval);
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let subscribe_deadline = tokio::time::sleep(config.io_timeout);
    tokio::pin!(subscribe_deadline);
    tokio::pin!(cancel);

    let mut line = Vec::new();
    let mut subscribed = false;
    let mut last_seq = None;
    let mut next_id = SUBSCRIBE_ID + 1;
    let mut pending_ping = None;
    let mut pong_deadline = None;
    let mut shutting_down = false;

    loop {
        // `select!` constructs disabled branch futures too, so give the no-ping case a
        // harmless distant instant instead of unwrapping the optional deadline.
        let next_pong_deadline =
            pong_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 60 * 60));
        tokio::select! {
            biased;
            cancel_result = &mut cancel => return cancel_result,
            _ = &mut subscribe_deadline, if !subscribed => {
                return Err(WatchError::transport(
                    "timed out waiting for the watch subscription reply.",
                ));
            }
            _ = tokio::time::sleep_until(next_pong_deadline), if pong_deadline.is_some() => {
                return Err(WatchError::transport("watch owner stopped answering pings."));
            }
            _ = ping_timer.tick() => {
                if pending_ping.is_some() {
                    continue;
                }
                let id = next_id;
                next_id = next_id.checked_add(1).ok_or_else(|| {
                    WatchError::transport("watch frame id overflowed.")
                })?;
                let ping = ClientFrame { id, op: ClientOp::Ping };
                write_json_line(&mut write_half, &ping, config.io_timeout, "ping").await?;
                pending_ping = Some(id);
                pong_deadline = Some(Instant::now() + config.io_timeout);
            }
            read_result = crate::util::io::read_bounded_line(
                &mut reader,
                &mut line,
                MAX_FRAME_BYTES,
            ) => {
                use crate::util::io::BoundedLine;

                let outcome = read_result.map_err(|error| {
                    WatchError::transport(format!("watch connection read failed: {error}"))
                })?;
                match outcome {
                    BoundedLine::TooLarge => {
                        return Err(WatchError::transport("watch owner sent an oversized frame."));
                    }
                    BoundedLine::Eof if shutting_down && line.is_empty() => return Ok(()),
                    BoundedLine::Eof if !line.is_empty() => {
                        return Err(WatchError::transport(
                            "watch owner closed with a malformed partial frame.",
                        ));
                    }
                    BoundedLine::Eof => {
                        return Err(WatchError::transport("watch connection closed unexpectedly."));
                    }
                    BoundedLine::Line => {}
                }

                let frame: ServerFrame = serde_json::from_slice(&line).map_err(|_| {
                    WatchError::transport("watch owner sent a malformed frame.")
                })?;
                line.clear();

                match &frame {
                    ServerFrame::Reply { id, resp } => {
                        if *id != SUBSCRIBE_ID || subscribed {
                            return Err(WatchError::transport(
                                "watch owner sent an unexpected reply.",
                            ));
                        }
                        if !resp.ok {
                            let reason = sanitize(resp.reason.as_deref().unwrap_or("rejected"), 64);
                            return Err(WatchError::transport(format!(
                                "watch subscription was rejected: {reason}."
                            )));
                        }
                        subscribed = true;
                    }
                    ServerFrame::Event { seq, topic, event } => {
                        validate_event(*seq, *topic, event, &config.topics, &mut last_seq)?;
                        emit_frame(&frame, config.json, config.quiet, emit)?;
                        if matches!(event, PushEvent::ShuttingDown) {
                            shutting_down = true;
                        }
                    }
                    ServerFrame::Pong { id } => {
                        if pending_ping != Some(*id) {
                            return Err(WatchError::transport(
                                "watch owner sent an unexpected pong.",
                            ));
                        }
                        pending_ping = None;
                        pong_deadline = None;
                    }
                    ServerFrame::Goodbye { reason } => {
                        emit_frame(&frame, config.json, config.quiet, emit)?;
                        if reason == "shutting_down" {
                            return Ok(());
                        }
                        return Err(WatchError::transport(format!(
                            "watch session ended: {}.",
                            sanitize(reason, 64)
                        )));
                    }
                }
            }
        }
    }
}

fn validate_ack(ack: &HelloAck) -> Result<(), WatchError> {
    if !ack.ok {
        let raw_reason = ack.reason.as_deref().unwrap_or("rejected");
        let reason = sanitize(raw_reason, 64);
        let message = format!("watch handshake was rejected: {reason}.");
        return if raw_reason == "bad_version" {
            Err(WatchError::unsupported(message))
        } else {
            Err(WatchError::transport(message))
        };
    }
    if ack.version != PROTOCOL_VERSION {
        return Err(WatchError::unsupported(format!(
            "watch handshake selected unsupported protocol {}.",
            ack.version
        )));
    }
    if ack.session_id == 0 || ack.reason.is_some() {
        return Err(WatchError::transport(
            "watch owner returned an invalid successful handshake.",
        ));
    }
    if !has_events_capability(&ack.capabilities) {
        return Err(WatchError::unsupported(
            "watch owner did not negotiate events-v8.",
        ));
    }
    Ok(())
}

fn validate_event(
    seq: u64,
    topic: Topic,
    event: &PushEvent,
    subscribed_topics: &[Topic],
    last_seq: &mut Option<u64>,
) -> Result<(), WatchError> {
    if !subscribed_topics.contains(&topic) || !event_matches_topic(topic, event) {
        return Err(WatchError::transport(
            "watch owner sent an event outside the subscription.",
        ));
    }

    let valid_seq = match *last_seq {
        None => seq == 1,
        Some(previous) => previous.checked_add(1) == Some(seq),
    };
    if !valid_seq {
        return Err(WatchError::transport(format!(
            "watch event sequence gap (last {}, received {seq}).",
            last_seq
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )));
    }
    *last_seq = Some(seq);
    Ok(())
}

fn event_matches_topic(topic: Topic, event: &PushEvent) -> bool {
    matches!(
        (topic, event),
        (Topic::Player, PushEvent::PlayerSnapshot { .. })
            | (Topic::Queue, PushEvent::QueueSnapshot { .. })
            | (Topic::Settings, PushEvent::SettingsSnapshot { .. })
            | (
                Topic::System,
                PushEvent::OwnerChanged { .. } | PushEvent::ShuttingDown
            )
    )
}

async fn write_json_line<W, T>(
    writer: &mut W,
    value: &T,
    write_timeout: Duration,
    label: &'static str,
) -> Result<(), WatchError>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut bytes = serde_json::to_vec(value).map_err(|error| {
        WatchError::transport(format!("could not encode watch {label}: {error}"))
    })?;
    bytes.push(b'\n');

    timeout(write_timeout, async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| WatchError::transport(format!("timed out writing watch {label}.")))?
    .map_err(|error| WatchError::transport(format!("could not write watch {label}: {error}")))
}

async fn read_json_line<R, T>(reader: &mut R, max_bytes: usize) -> Result<T, WatchError>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    use crate::util::io::BoundedLine;

    let mut line = Vec::new();
    match crate::util::io::read_bounded_line(reader, &mut line, max_bytes)
        .await
        .map_err(|error| WatchError::transport(format!("watch handshake read failed: {error}")))?
    {
        BoundedLine::Line => serde_json::from_slice(&line)
            .map_err(|_| WatchError::transport("watch owner sent a malformed handshake.")),
        BoundedLine::TooLarge => Err(WatchError::transport(
            "watch owner sent an oversized handshake.",
        )),
        BoundedLine::Eof => Err(WatchError::transport(
            "watch connection closed during the handshake.",
        )),
    }
}

fn emit_frame<E>(
    frame: &ServerFrame,
    json: bool,
    quiet: bool,
    emit: &mut E,
) -> Result<(), WatchError>
where
    E: FnMut(&str) -> io::Result<()>,
{
    let line = if json {
        serde_json::to_string(frame)
            .map_err(|error| WatchError::transport(format!("could not encode event: {error}")))?
    } else if quiet {
        return Ok(());
    } else {
        human_line(frame).ok_or_else(|| WatchError::transport("could not render watch event."))?
    };
    emit(&line).map_err(|error| WatchError::transport(format!("could not write output: {error}")))
}

fn human_line(frame: &ServerFrame) -> Option<String> {
    match frame {
        ServerFrame::Event { event, .. } => match event {
            PushEvent::PlayerSnapshot { model } => {
                let state = if model.paused { "paused" } else { "playing" };
                let track = model.track.as_ref().map_or_else(
                    || "nothing playing".to_string(),
                    |track| {
                        let title = track
                            .display_title
                            .as_deref()
                            .unwrap_or(track.title.as_str());
                        let artist = track
                            .display_artist
                            .as_deref()
                            .unwrap_or(track.artist.as_str());
                        format!("{} — {}", sanitize(title, 80), sanitize(artist, 60))
                    },
                );
                let queue = if model.queue_len == 0 {
                    "queue empty".to_string()
                } else {
                    format!(
                        "queue {}/{}",
                        model.queue_pos.saturating_add(1),
                        model.queue_len
                    )
                };
                Some(format!(
                    "[player] {state} · {track} · vol {}% · {queue}",
                    model.volume
                ))
            }
            PushEvent::QueueSnapshot { model } => Some(format!(
                "[queue] {} {} · rev {}",
                model.items.len(),
                if model.items.len() == 1 {
                    "track"
                } else {
                    "tracks"
                },
                model.rev
            )),
            PushEvent::SettingsSnapshot { model } => Some(format!(
                "[settings] rev {} · speed {:.1}x · autoplay {} · source {} · mode {}",
                model.rev,
                f32::from(model.playback.speed_tenths) / 10.0,
                on_off(model.streaming.autoplay),
                model.search.default_source.label(),
                sanitize(&model.streaming.mode, 32)
            )),
            PushEvent::OwnerChanged { mode } => Some(format!(
                "[system] owner {}",
                match mode {
                    super::proto::InstanceMode::StandaloneTui => "standalone_tui",
                    super::proto::InstanceMode::Daemon => "daemon",
                }
            )),
            PushEvent::ShuttingDown => Some("[system] shutting down".to_string()),
            _ => None,
        },
        ServerFrame::Goodbye { reason } => {
            Some(format!("[system] goodbye · {}", sanitize(reason, 64)))
        }
        ServerFrame::Reply { .. } | ServerFrame::Pong { .. } => None,
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn sanitize(value: &str, max_chars: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            let unsafe_format = character.is_control()
                || matches!(
                    character,
                    '\u{200e}'
                        | '\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                );
            if unsafe_format { ' ' } else { character }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let mut chars = cleaned.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;
    use crate::remote::proto::{InstanceMode, RemoteResponse};

    struct StallingWriter {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for StallingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                self.release.recv().map_err(io::Error::other)?;
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct RecordingWriter {
        bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn config(topics: Vec<Topic>) -> SessionConfig {
        SessionConfig {
            token: "0123456789abcdef0123456789abcdef".to_string(),
            topics,
            json: true,
            quiet: false,
            io_timeout: Duration::from_secs(1),
            ping_interval: Duration::from_secs(30),
        }
    }

    fn ack() -> HelloAck {
        HelloAck {
            ok: true,
            version: PROTOCOL_VERSION,
            session_id: 7,
            capabilities: vec!["events-v8".to_string()],
            owner_mode: InstanceMode::Daemon,
            reason: None,
        }
    }

    async fn server_handshake<S>(
        stream: S,
        expected_topics: &[Topic],
    ) -> (BufReader<tokio::io::ReadHalf<S>>, tokio::io::WriteHalf<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let hello: HelloRequest = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.token, "0123456789abcdef0123456789abcdef");
        assert_eq!(hello.hello.client, "ytt-cli-watch");
        assert_eq!(hello.hello.min_version, PROTOCOL_VERSION);
        write_test_line(&mut write_half, &ack()).await;

        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let subscribe: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(subscribe.id, SUBSCRIBE_ID);
        assert_eq!(
            subscribe.op,
            ClientOp::Subscribe {
                topics: expected_topics.to_vec()
            }
        );
        (reader, write_half)
    }

    async fn write_test_line<W: AsyncWrite + Unpin, T: serde::Serialize>(
        writer: &mut W,
        value: &T,
    ) {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
        writer.flush().await.unwrap();
    }

    fn system_event(seq: u64, event: PushEvent) -> ServerFrame {
        ServerFrame::Event {
            seq,
            topic: Topic::System,
            event,
        }
    }

    #[tokio::test]
    async fn immediate_cancel_wins_before_operation_is_polled() {
        let operation_polled = std::cell::Cell::new(false);
        let operation = std::future::poll_fn(|_| {
            operation_polled.set(true);
            std::task::Poll::Ready(EXIT_TRANSPORT)
        });
        let cancel = std::future::ready(Ok::<(), io::Error>(()));

        assert_eq!(race_operation_with_cancel(operation, cancel).await, EXIT_OK);
        assert!(!operation_polled.get());
    }

    #[tokio::test]
    async fn cancel_during_pre_ack_handshake_exits_successfully() {
        let (client, server) = tokio::io::duplex(4096);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let operation = async move {
            let mut emit = |_line: &str| Ok(());
            match run_session(
                client,
                config(vec![Topic::System]),
                pending::<Result<(), WatchError>>(),
                &mut emit,
            )
            .await
            {
                Ok(()) => EXIT_OK,
                Err(error) => error.exit_code(),
            }
        };
        let cancel = async move {
            cancel_rx.await.map_err(io::Error::other)?;
            Ok(())
        };
        let server_fut = async move {
            let (read_half, _write_half) = tokio::io::split(server);
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            serde_json::from_str::<HelloRequest>(line.trim()).unwrap();
            cancel_tx.send(()).unwrap();

            line.clear();
            assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
        };

        let (exit_code, ()) =
            tokio::join!(race_operation_with_cancel(operation, cancel), server_fut);
        assert_eq!(exit_code, EXIT_OK);
    }

    #[tokio::test]
    async fn outer_cancel_interrupts_stalled_output_drain() {
        let (client, server) = tokio::io::duplex(4096);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = StallingWriter {
            started: Some(started_tx),
            release: release_rx,
        };
        let operation = async move {
            let _ = run_session_with_output(
                client,
                config(vec![Topic::System]),
                pending::<Result<(), WatchError>>(),
                writer,
                4,
                Duration::from_secs(5),
            )
            .await;
            EXIT_TRANSPORT
        };
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let cancel = async move {
            cancel_rx.await.map_err(io::Error::other)?;
            Ok(())
        };
        let server_fut = async move {
            let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut socket_writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Goodbye {
                    reason: "shutting_down".to_string(),
                },
            )
            .await;

            started_rx.await.unwrap();
            cancel_tx.send(()).unwrap();
            tokio::task::yield_now().await;
            release_tx.send(()).unwrap();
        };

        let (exit_code, ()) =
            tokio::join!(race_operation_with_cancel(operation, cancel), server_fut);
        assert_eq!(exit_code, EXIT_OK);
    }

    #[tokio::test]
    async fn stalled_output_flush_times_out_as_transport_error() {
        let (client, server) = tokio::io::duplex(4096);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = StallingWriter {
            started: Some(started_tx),
            release: release_rx,
        };
        let client_fut = run_session_with_output(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            writer,
            4,
            Duration::from_millis(50),
        );
        let server_fut = async move {
            let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut socket_writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Goodbye {
                    reason: "shutting_down".to_string(),
                },
            )
            .await;
            started_rx.await.unwrap();
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        release_tx.send(()).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("timed out flushing"));
    }

    #[tokio::test]
    async fn full_output_lane_is_nonblocking_and_reports_slow_consumer() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = StallingWriter {
            started: Some(started_tx),
            release: release_rx,
        };
        let (line_tx, mut done_rx) = spawn_output_writer(writer, 1).unwrap();

        enqueue_output_line(&line_tx, "first").unwrap();
        started_rx.await.unwrap();
        enqueue_output_line(&line_tx, "second").unwrap();
        let error = enqueue_output_line(&line_tx, "third").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        drop(line_tx);
        release_tx.send(()).unwrap();
        let completion = timeout(Duration::from_secs(1), &mut done_rx).await.unwrap();
        assert_eq!(output_completion(completion), Ok(()));
    }

    #[tokio::test]
    async fn broken_output_writer_is_transport_error() {
        let (client, server) = tokio::io::duplex(4096);
        let client_fut = run_session_with_output(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            BrokenWriter,
            4,
            Duration::from_secs(1),
        );
        let server_fut = async move {
            let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut socket_writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("output"));
    }

    #[tokio::test]
    async fn abnormal_goodbye_drains_accepted_output_in_order() {
        let (client, server) = tokio::io::duplex(4096);
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = RecordingWriter {
            bytes: std::sync::Arc::clone(&bytes),
        };
        let client_fut = run_session_with_output(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            writer,
            4,
            Duration::from_secs(1),
        );
        let event = system_event(
            1,
            PushEvent::OwnerChanged {
                mode: InstanceMode::Daemon,
            },
        );
        let goodbye = ServerFrame::Goodbye {
            reason: "slow_consumer".to_string(),
        };
        let expected = format!(
            "{}\n{}\n",
            serde_json::to_string(&event).unwrap(),
            serde_json::to_string(&goodbye).unwrap()
        );
        let server_fut = async move {
            let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(&mut socket_writer, &event).await;
            write_test_line(
                &mut socket_writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(&mut socket_writer, &goodbye).await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert!(error.message.contains("slow_consumer"));
        let recorded = bytes.lock().unwrap().clone();
        assert_eq!(std::str::from_utf8(&recorded).unwrap(), expected);
    }

    #[tokio::test]
    async fn accepts_snapshot_before_subscribe_reply_and_json_filters_frames() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let mut lines = Vec::new();
        let mut emit = |line: &str| -> io::Result<()> {
            lines.push(line.to_string());
            Ok(())
        };
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(&mut writer, &system_event(2, PushEvent::ShuttingDown)).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Goodbye {
                    reason: "shutting_down".to_string(),
                },
            )
            .await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        assert_eq!(result, Ok(()));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""seq":1"#));
        assert!(lines[1].contains(r#""kind":"shutting_down""#));
        assert_eq!(lines[2], r#"{"frame":"goodbye","reason":"shutting_down"}"#);
        assert!(
            lines
                .iter()
                .all(|line| !line.contains(r#""frame":"reply""#))
        );
    }

    #[tokio::test]
    async fn sends_ping_and_suppresses_pong() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let mut test_config = config(vec![Topic::System]);
        test_config.ping_interval = Duration::from_millis(10);
        let mut lines = Vec::new();
        let mut emit = |line: &str| -> io::Result<()> {
            lines.push(line.to_string());
            Ok(())
        };
        let client_fut = run_session(
            client,
            test_config,
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (mut reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;

            let mut line = String::new();
            timeout(Duration::from_secs(1), reader.read_line(&mut line))
                .await
                .expect("client should ping")
                .unwrap();
            let ping: ClientFrame = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(ping.id, SUBSCRIBE_ID + 1);
            assert_eq!(ping.op, ClientOp::Ping);
            write_test_line(&mut writer, &ServerFrame::Pong { id: ping.id }).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Goodbye {
                    reason: "shutting_down".to_string(),
                },
            )
            .await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        assert_eq!(result, Ok(()));
        assert_eq!(
            lines,
            vec![r#"{"frame":"goodbye","reason":"shutting_down"}"#]
        );
    }

    #[tokio::test]
    async fn rejects_event_sequence_gap() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(&mut writer, &system_event(3, PushEvent::ShuttingDown)).await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("sequence gap"));
    }

    #[tokio::test]
    async fn rejects_duplicate_event_sequence() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(&mut writer, &system_event(1, PushEvent::ShuttingDown)).await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("sequence gap"));
    }

    #[tokio::test]
    async fn malformed_regular_frame_is_transport_error() {
        let (client, server) = tokio::io::duplex(4096);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            writer.write_all(b"{not-json}\n").await.unwrap();
            writer.flush().await.unwrap();
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("malformed frame"));
    }

    #[tokio::test]
    async fn partial_eof_after_shutting_down_is_transport_error() {
        let (client, server) = tokio::io::duplex(4096);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &system_event(
                    1,
                    PushEvent::OwnerChanged {
                        mode: InstanceMode::Daemon,
                    },
                ),
            )
            .await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(&mut writer, &system_event(2, PushEvent::ShuttingDown)).await;
            writer.write_all(b"{\"frame\":\"goodbye\"").await.unwrap();
            writer.flush().await.unwrap();
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("malformed partial frame"));
    }

    #[tokio::test]
    async fn rejects_oversized_frame_without_waiting_for_newline() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
            let _ = writer.write_all(&oversized).await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("oversized"));
    }

    #[tokio::test]
    async fn capacity_handshake_rejection_is_transport_error() {
        let (client, server) = tokio::io::duplex(4096);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (read_half, mut write_half) = tokio::io::split(server);
            let mut reader = BufReader::new(read_half);
            let mut hello = String::new();
            reader.read_line(&mut hello).await.unwrap();
            let rejected = HelloAck {
                ok: false,
                version: PROTOCOL_VERSION,
                session_id: 0,
                capabilities: Vec::new(),
                owner_mode: InstanceMode::Daemon,
                reason: Some("sessions_full".to_string()),
            };
            write_test_line(&mut write_half, &rejected).await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("sessions_full"));
    }

    #[test]
    fn handshake_rejection_reason_is_sanitized_and_bounded() {
        let rejected = HelloAck {
            ok: false,
            version: PROTOCOL_VERSION,
            session_id: 0,
            capabilities: Vec::new(),
            owner_mode: InstanceMode::Daemon,
            reason: Some(format!(
                "sessions_full\n\u{1b}]0;watch-owned-{}\u{7}",
                "x".repeat(100)
            )),
        };

        let error = validate_ack(&rejected).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(!error.message.chars().any(char::is_control));
        assert!(error.message.contains('…'));
    }

    #[tokio::test]
    async fn subscription_rejection_reason_is_sanitized_and_bounded() {
        let (client, server) = tokio::io::duplex(4096);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            let reason = format!("rejected\r\n\u{1b}]0;watch-owned-{}\u{7}", "y".repeat(100));
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::err(&reason),
                },
            )
            .await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(!error.message.chars().any(char::is_control));
        assert!(error.message.contains('…'));
    }

    #[test]
    fn protocol_handshake_rejection_is_usage_error() {
        let rejected = HelloAck {
            ok: false,
            version: PROTOCOL_VERSION,
            session_id: 0,
            capabilities: Vec::new(),
            owner_mode: InstanceMode::Daemon,
            reason: Some("bad_version".to_string()),
        };
        let error = validate_ack(&rejected).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(error.message.contains("bad_version"));
    }

    #[tokio::test]
    async fn unanswered_ping_fails_on_io_deadline() {
        let (client, server) = tokio::io::duplex(4096);
        let mut test_config = config(vec![Topic::System]);
        test_config.io_timeout = Duration::from_millis(100);
        test_config.ping_interval = Duration::from_millis(10);
        let mut emit = |_line: &str| Ok(());
        let client_fut = run_session(
            client,
            test_config,
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (mut reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;

            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let ping: ClientFrame = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(ping.op, ClientOp::Ping);
            line.clear();
            assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("answering pings"));
    }

    #[tokio::test]
    async fn slow_consumer_and_idle_goodbyes_are_transport_errors() {
        for reason in ["slow_consumer", "idle_timeout"] {
            let (client, server) = tokio::io::duplex(4096);
            let mut lines = Vec::new();
            let mut emit = |line: &str| -> io::Result<()> {
                lines.push(line.to_string());
                Ok(())
            };
            let client_fut = run_session(
                client,
                config(vec![Topic::System]),
                pending::<Result<(), WatchError>>(),
                &mut emit,
            );
            let server_fut = async move {
                let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
                write_test_line(
                    &mut writer,
                    &ServerFrame::Reply {
                        id: SUBSCRIBE_ID,
                        resp: RemoteResponse::ok("subscribed".to_string()),
                    },
                )
                .await;
                write_test_line(
                    &mut writer,
                    &ServerFrame::Goodbye {
                        reason: reason.to_string(),
                    },
                )
                .await;
            };

            let (result, ()) = tokio::join!(client_fut, server_fut);
            let error = result.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Transport);
            assert!(error.message.contains(reason));
            assert_eq!(lines.len(), 1, "JSON should retain the Goodbye frame");
            assert!(lines[0].contains(reason));
        }
    }

    #[test]
    fn topic_validation_deduplicates_and_rejects_non_watch_topics() {
        assert_eq!(
            normalize_topics(vec![Topic::Player, Topic::Player, Topic::System]).unwrap(),
            vec![Topic::Player, Topic::System]
        );
        let error = normalize_topics(vec![Topic::Lyrics]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn first_event_sequence_must_start_at_one() {
        let mut last_seq = None;
        let error = validate_event(
            2,
            Topic::System,
            &PushEvent::OwnerChanged {
                mode: InstanceMode::Daemon,
            },
            &[Topic::System],
            &mut last_seq,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains("sequence gap"));
        assert_eq!(last_seq, None);
    }

    #[test]
    fn sanitizer_removes_terminal_controls_and_bounds_fields() {
        assert_eq!(sanitize("song\n\u{1b}[31m  title", 80), "song [31m title");
        assert_eq!(sanitize("left\u{202e}right", 80), "left right");
        assert_eq!(sanitize("abcdef", 3), "abc…");
    }

    #[test]
    fn quiet_suppresses_human_output_but_json_wins() {
        let frame = system_event(
            1,
            PushEvent::OwnerChanged {
                mode: InstanceMode::Daemon,
            },
        );
        let mut lines = Vec::new();
        {
            let mut emit = |line: &str| -> io::Result<()> {
                lines.push(line.to_string());
                Ok(())
            };
            emit_frame(&frame, false, true, &mut emit).unwrap();
        }
        assert!(lines.is_empty());
        {
            let mut emit = |line: &str| -> io::Result<()> {
                lines.push(line.to_string());
                Ok(())
            };
            emit_frame(&frame, true, true, &mut emit).unwrap();
        }
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(r#""frame":"event""#));
    }
}
