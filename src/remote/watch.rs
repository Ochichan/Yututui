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
#[path = "watch_tests.rs"]
mod tests;
