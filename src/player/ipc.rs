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
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Duration, sleep};

use super::proto::{self, MpvIncoming};
use super::{EventSink, PlayerCmd, PlayerEvent};

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
/// mpv events to the runtime (`emit`) and writing player commands (`cmd_rx`) to mpv.
/// Returns when mpv closes the connection or all command senders drop.
pub async fn run_actor(conn: Stream, mut cmd_rx: UnboundedReceiver<PlayerCmd>, emit: EventSink) {
    // Subscribe to the properties the player view needs. IDs are arbitrary but stable.
    for (id, prop) in [
        (1u64, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "volume"),
        (5, "metadata"),
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
                Ok(_) => dispatch_incoming(&line, &emit, &mut last_sent_time_sec),
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

/// Borrowed view of the one high-rate mpv line: a `time-pos` property-change. Parsing
/// into this (zero-copy `&str` fields, `data` as a plain number) skips the full
/// `serde_json::Value` tree that `proto::parse_line` builds — and time-pos arrives many
/// times a second during playback, then is deduped to 1/sec anyway.
#[derive(serde::Deserialize)]
struct TimePosLine<'a> {
    event: &'a str,
    name: &'a str,
    data: f64,
}

/// Translate one mpv line into a player event for the runtime.
fn dispatch_incoming(line: &str, emit: &EventSink, last_sent_time_sec: &mut Option<i64>) {
    // Fast path: dedup time-pos before allocating anything. A borrow-mode parse fails on
    // any other event shape (missing fields, null/escaped data) and falls through to the
    // general path below, so behavior is unchanged — e.g. `data:null` still ends up in
    // the `as_f64() == None` arm there and emits nothing.
    if let Ok(tp) = serde_json::from_str::<TimePosLine>(line.trim())
        && tp.event == "property-change"
        && tp.name == "time-pos"
    {
        let sec = tp.data as i64;
        if *last_sent_time_sec != Some(sec) {
            *last_sent_time_sec = Some(sec);
            emit(PlayerEvent::TimePos(tp.data));
        }
        return;
    }
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
                        emit(PlayerEvent::TimePos(t));
                    }
                }
            }
            "duration" => {
                if let Some(d) = value.as_f64() {
                    emit(PlayerEvent::Duration(d));
                }
            }
            "pause" => {
                if let Some(p) = value.as_bool() {
                    emit(PlayerEvent::Paused(p));
                }
            }
            "volume" => {
                if let Some(v) = value.as_f64() {
                    emit(PlayerEvent::Volume(v));
                }
            }
            "metadata" => {
                emit(PlayerEvent::Metadata(value));
            }
            _ => {}
        },
        MpvIncoming::EndFile { reason, file_error } => match reason.as_str() {
            "eof" => {
                *last_sent_time_sec = None;
                emit(PlayerEvent::Eof);
            }
            "error" => {
                *last_sent_time_sec = None;
                // Surface mpv's own reason when it gives one — it's the closest thing to a
                // "why" (HTTP 403, unsupported format, …); otherwise a generic message.
                let msg = match file_error {
                    Some(fe) if !fe.is_empty() => format!("mpv could not play this track ({fe})"),
                    _ => "mpv could not play this track".to_owned(),
                };
                emit(PlayerEvent::Error(msg));
            }
            _ => {}
        },
        MpvIncoming::Other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_property_change_is_forwarded() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut last_sent_time_sec = None;

        dispatch_incoming(
            r#"{"event":"property-change","id":5,"name":"metadata","data":{"icy-title":"Artist - Track"}}"#,
            &emit,
            &mut last_sent_time_sec,
        );

        match rx.try_recv().expect("metadata message") {
            PlayerEvent::Metadata(value) => {
                assert_eq!(value["icy-title"], "Artist - Track");
            }
            _ => panic!("expected metadata event"),
        }
    }

    #[test]
    fn time_pos_dedups_to_whole_seconds() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut last_sent_time_sec = None;

        for data in ["1.1", "1.4", "1.9", "2.0"] {
            let line =
                format!(r#"{{"event":"property-change","id":1,"name":"time-pos","data":{data}}}"#);
            dispatch_incoming(&line, &emit, &mut last_sent_time_sec);
        }

        // 1.1 emits (second 1), 1.4/1.9 dedup away, 2.0 emits (second 2).
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::TimePos(t)) if t == 1.1));
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::TimePos(t)) if t == 2.0));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn null_time_pos_emits_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut last_sent_time_sec = Some(3);

        dispatch_incoming(
            r#"{"event":"property-change","id":1,"name":"time-pos","data":null}"#,
            &emit,
            &mut last_sent_time_sec,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(last_sent_time_sec, Some(3));
    }
}
