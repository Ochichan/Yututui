//! The mpv IPC client: connect to the socket/pipe, then run the actor loop.
//!
//! `interprocess` gives us one async `LocalSocket` API over Unix sockets and Windows
//! named pipes. mpv creates the endpoint a moment after spawn, so [`connect_retry`]
//! polls briefly. The actor holds a single connection and reads events while writing
//! commands concurrently by sharing `&conn` (the documented `interprocess` pattern).

use std::io;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{Duration, sleep};

use super::PlayerCmd;
use super::proto::{self, MpvIncoming};
use crate::app::Msg;

/// Connect to the mpv IPC endpoint, retrying for ~3s while mpv finishes starting up.
pub async fn connect_retry(path: &str) -> io::Result<Stream> {
    let mut last_err: Option<io::Error> = None;
    for _ in 0..200 {
        let name = path.to_fs_name::<GenericFilePath>()?;
        match Stream::connect(name).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(15)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "mpv IPC connect timed out")))
}

/// Drive one mpv connection: subscribe to progress properties, then loop forwarding
/// mpv events to the UI (`msg_tx`) and writing player commands (`cmd_rx`) to mpv.
/// Returns when mpv closes the connection or all command senders drop.
pub async fn run_actor(
    conn: Stream,
    mut cmd_rx: UnboundedReceiver<PlayerCmd>,
    msg_tx: UnboundedSender<Msg>,
) {
    // Subscribe to the properties the player view needs. IDs are arbitrary but stable.
    for (id, prop) in [
        (1u64, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "volume"),
    ] {
        if let Err(e) = write_json(&conn, &proto::cmd_observe(id, prop)).await {
            tracing::warn!(error = %e, property = prop, "failed to observe mpv property");
        }
    }

    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    let mut request_id: u64 = 10;
    let mut last_sent_time_sec: Option<i64> = None;

    loop {
        line.clear();
        tokio::select! {
            read = reader.read_line(&mut line) => match read {
                Ok(0) | Err(_) => break, // mpv closed the connection
                Ok(_) => dispatch_incoming(&line, &msg_tx, &mut last_sent_time_sec),
            },
            cmd = cmd_rx.recv() => match cmd {
                None => break, // all senders dropped: shutting down
                Some(cmd) => {
                    request_id += 1;
                    let json = match cmd {
                        PlayerCmd::Load(url) => {
                            last_sent_time_sec = None;
                            proto::cmd_loadfile(&url, "replace", request_id)
                        },
                        PlayerCmd::Stop => {
                            last_sent_time_sec = None;
                            proto::cmd_stop(request_id)
                        },
                        PlayerCmd::CyclePause => proto::cmd_cycle("pause", request_id),
                        PlayerCmd::SeekRelative(secs) => proto::cmd_seek_relative(secs, request_id),
                        PlayerCmd::SeekAbsolute(secs) => proto::cmd_seek_absolute(secs, request_id),
                        PlayerCmd::SetVolume(vol) => proto::cmd_set_volume(vol, request_id),
                        PlayerCmd::SetAudioFilter(af) => {
                            proto::cmd_set_property("af", &serde_json::Value::from(af), request_id)
                        }
                        PlayerCmd::AfCommand { label, param, value } => {
                            proto::cmd_af_command(&label, &param, &value, request_id)
                        }
                        PlayerCmd::SetProperty { name, value } => {
                            proto::cmd_set_property(&name, &value, request_id)
                        }
                    };
                    if let Err(e) = write_json(&conn, &json).await {
                        tracing::warn!(error = %e, "failed to write mpv command");
                    }
                }
            },
        }
    }
}

/// Write one newline-terminated JSON command to mpv.
async fn write_json(conn: &Stream, json: &str) -> io::Result<()> {
    let mut writer = conn;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Translate one mpv line into a [`Msg`] for the reducer.
fn dispatch_incoming(line: &str, tx: &UnboundedSender<Msg>, last_sent_time_sec: &mut Option<i64>) {
    let Some(incoming) = proto::parse_line(line) else {
        return;
    };
    match incoming {
        MpvIncoming::PropertyChange { name, value } => match name.as_str() {
            "time-pos" => {
                if let Some(t) = value.as_f64() {
                    let sec = t as i64;
                    if *last_sent_time_sec != Some(sec) {
                        *last_sent_time_sec = Some(sec);
                        let _ = tx.send(Msg::PlayerTimePos(t));
                    }
                }
            }
            "duration" => {
                if let Some(d) = value.as_f64() {
                    let _ = tx.send(Msg::PlayerDuration(d));
                }
            }
            "pause" => {
                if let Some(p) = value.as_bool() {
                    let _ = tx.send(Msg::PlayerPaused(p));
                }
            }
            "volume" => {
                if let Some(v) = value.as_f64() {
                    let _ = tx.send(Msg::PlayerVolume(v));
                }
            }
            _ => {}
        },
        MpvIncoming::EndFile { reason, file_error } => match reason.as_str() {
            "eof" => {
                *last_sent_time_sec = None;
                let _ = tx.send(Msg::PlayerEof);
            }
            "error" => {
                *last_sent_time_sec = None;
                // Surface mpv's own reason when it gives one — it's the closest thing to a
                // "why" (HTTP 403, unsupported format, …); otherwise a generic message.
                let msg = match file_error {
                    Some(fe) if !fe.is_empty() => format!("mpv could not play this track ({fe})"),
                    _ => "mpv could not play this track".to_owned(),
                };
                let _ = tx.send(Msg::PlayerError(msg));
            }
            _ => {}
        },
        MpvIncoming::Other => {}
    }
}
