//! The mpv IPC client: connect to the socket/pipe, then run the actor loop.
//!
//! `interprocess` gives us one async `LocalSocket` API over Unix sockets and Windows
//! named pipes. mpv creates the endpoint a moment after spawn, so [`connect_retry`]
//! polls briefly. The actor holds a single connection and reads events while writing
//! commands concurrently by sharing `&conn` (the documented `interprocess` pattern).

use std::collections::HashMap;
use std::io;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Duration, sleep};

use super::proto::{self, MpvIncoming};
use super::{EventSink, PlayerCmd, PlayerEvent};

/// Upper bound on a single mpv IPC line. mpv's JSON events are well under a kilobyte; the
/// cap only guards against a broken/hostile endpoint growing one line without limit.
pub(crate) const MPV_IPC_MAX_LINE: usize = 1024 * 1024;

#[derive(Default)]
struct DispatchState {
    last_sent_time_sec: Option<i64>,
    last_sent_cache_sec: Option<i64>,
    /// Whether a real (numeric) duration was forwarded for the current file — the latch
    /// that turns the FIRST `duration: null` into a `Duration(None)` loss signal while
    /// keeping repeated nulls (a stream that never had one) silent.
    duration_known: bool,
    pending: HashMap<u64, String>,
}

/// Clear the per-file dedup/latch state when a file ends for ANY reason, so the next
/// load's first `time-pos`/cache second and duration are never dedup-suppressed.
fn reset_file_state(state: &mut DispatchState) {
    state.last_sent_time_sec = None;
    state.last_sent_cache_sec = None;
    state.duration_known = false;
}

fn remember_pending_command(state: &mut DispatchState, request_id: u64, label: impl Into<String>) {
    if state.pending.len() >= 128
        && let Some(oldest) = state.pending.keys().min().copied()
    {
        state.pending.remove(&oldest);
    }
    state.pending.insert(request_id, label.into());
}

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
    let mut state = DispatchState::default();

    // Subscribe to the properties the player view needs. IDs are arbitrary but stable.
    for (id, prop) in [
        (1u64, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "volume"),
        (5, "metadata"),
        (6, "demuxer-cache-time"),
        // For the radio recorder: pick the passthrough container from the stream codec.
        (7, "audio-codec-name"),
        (8, "file-format"),
    ] {
        remember_pending_command(&mut state, id, format!("observe {prop}"));
        if let Err(e) = write_json(&conn, &proto::cmd_observe(id, prop)).await {
            tracing::warn!(error = %e, property = prop, "failed to observe mpv property");
        }
    }

    let mut reader = BufReader::new(&conn);
    let mut line: Vec<u8> = Vec::new();
    let mut request_id: u64 = 10;

    loop {
        line.clear();
        tokio::select! {
            // Bounded read (shared with the remote protocol): a well-behaved mpv sends tiny
            // JSON lines, so a line past the cap means a broken/hostile endpoint — tear down
            // rather than let one line grow memory without limit.
            read = crate::util::io::read_bounded_line(&mut reader, &mut line, MPV_IPC_MAX_LINE) => match read {
                Ok(crate::util::io::BoundedLine::Eof) | Err(_) => break, // mpv closed / transport error
                Ok(crate::util::io::BoundedLine::TooLarge) => {
                    tracing::warn!(cap = MPV_IPC_MAX_LINE, "mpv IPC line exceeded cap; closing");
                    break;
                }
                Ok(crate::util::io::BoundedLine::Line) => {
                    let text = String::from_utf8_lossy(&line);
                    dispatch_incoming(&text, &emit, &mut state);
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                None => break, // all senders dropped: shutting down
                Some(cmd) => {
                    request_id += 1;
                    let (json, label) = match cmd {
                        PlayerCmd::Load(url) => {
                            state.last_sent_time_sec = None;
                            state.last_sent_cache_sec = None;
                            state.duration_known = false;
                            (proto::cmd_loadfile(&url, "replace", request_id), "loadfile".to_owned())
                        },
                        PlayerCmd::Stop => {
                            state.last_sent_time_sec = None;
                            state.last_sent_cache_sec = None;
                            state.duration_known = false;
                            (proto::cmd_stop(request_id), "stop".to_owned())
                        },
                        PlayerCmd::CyclePause => {
                            (proto::cmd_cycle("pause", request_id), "cycle pause".to_owned())
                        }
                        PlayerCmd::SeekRelative(secs) => {
                            (proto::cmd_seek_relative(secs, request_id), "seek".to_owned())
                        }
                        PlayerCmd::SeekAbsolute(secs) => {
                            (proto::cmd_seek_absolute(secs, request_id), "seek".to_owned())
                        }
                        PlayerCmd::SetVolume(vol) => {
                            (proto::cmd_set_volume(vol, request_id), "set volume".to_owned())
                        }
                        PlayerCmd::SetAudioFilter(af) => {
                            (
                                proto::cmd_set_property("af", &serde_json::Value::from(af), request_id),
                                "set af".to_owned(),
                            )
                        }
                        PlayerCmd::AfCommand { label, param, value } => {
                            (
                                proto::cmd_af_command(&label, &param, &value, request_id),
                                "af-command".to_owned(),
                            )
                        }
                        PlayerCmd::SetProperty { name, value } => {
                            let label = format!("set_property {name}");
                            (proto::cmd_set_property(&name, &value, request_id), label)
                        }
                    };
                    remember_pending_command(&mut state, request_id, label);
                    if let Err(e) = write_json(&conn, &json).await {
                        tracing::warn!(error = %e, "failed to write mpv command");
                    }
                }
            },
        }
    }
}

/// Write one newline-terminated JSON command to mpv. Shared with the video-overlay
/// client ([`super::video`]), which drives a second mpv over the same wire format.
pub(super) async fn write_json(conn: &Stream, json: &str) -> io::Result<()> {
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
fn dispatch_incoming(line: &str, emit: &EventSink, state: &mut DispatchState) {
    // Fast path: dedup time-pos before allocating anything. A borrow-mode parse fails on
    // any other event shape (missing fields, null/escaped data) and falls through to the
    // general path below, so behavior is unchanged — e.g. `data:null` still ends up in
    // the `as_f64() == None` arm there and emits nothing.
    if let Ok(tp) = serde_json::from_str::<TimePosLine>(line.trim())
        && tp.event == "property-change"
        && tp.name == "time-pos"
    {
        let sec = tp.data as i64;
        if state.last_sent_time_sec != Some(sec) {
            state.last_sent_time_sec = Some(sec);
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
                    if state.last_sent_time_sec != Some(sec) {
                        state.last_sent_time_sec = Some(sec);
                        emit(PlayerEvent::TimePos(t));
                    }
                }
            }
            "duration" => match value.as_f64() {
                Some(d) => {
                    state.duration_known = true;
                    emit(PlayerEvent::Duration(Some(d)));
                }
                // Null after a real value = the property became unavailable (live edge,
                // mid-file teardown) — forward the loss ONCE so a reducer that missed the
                // load-time clear (late event from the old file) can't keep a stale length.
                None => {
                    if std::mem::take(&mut state.duration_known) {
                        emit(PlayerEvent::Duration(None));
                    }
                }
            },
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
            "audio-codec-name" => {
                emit(PlayerEvent::AudioCodec(value.as_str().map(str::to_owned)));
            }
            "file-format" => {
                emit(PlayerEvent::FileFormat(value.as_str().map(str::to_owned)));
            }
            "demuxer-cache-time" => match value.as_f64() {
                // High-rate like time-pos → dedup to whole seconds.
                Some(t) => {
                    let sec = t as i64;
                    if state.last_sent_cache_sec != Some(sec) {
                        state.last_sent_cache_sec = Some(sec);
                        emit(PlayerEvent::CacheTime(Some(t)));
                    }
                }
                // Unlike time-pos, a null here is a signal the reducer needs: the
                // property became unavailable (stream teardown, cache-less demuxer).
                None => {
                    if state.last_sent_cache_sec.take().is_some() {
                        emit(PlayerEvent::CacheTime(None));
                    }
                }
            },
            _ => {}
        },
        MpvIncoming::EndFile { reason, file_error } => match reason.as_str() {
            "eof" => {
                reset_file_state(state);
                emit(PlayerEvent::Eof);
            }
            "error" => {
                reset_file_state(state);
                // Surface mpv's own reason when it gives one — it's the closest thing to a
                // "why" (HTTP 403, unsupported format, …); otherwise a generic message.
                let msg = match file_error {
                    Some(fe) if !fe.is_empty() => format!("mpv could not play this track ({fe})"),
                    _ => "mpv could not play this track".to_owned(),
                };
                emit(PlayerEvent::Error(msg));
            }
            // `stop` (our own Stop/replace), `quit`, `redirect`, and any future reason:
            // no event (the reducers own those transitions), but the per-file dedup
            // state must not survive into the next load's first second.
            reason => {
                reset_file_state(state);
                tracing::debug!(reason, "mpv end-file");
            }
        },
        MpvIncoming::CommandReply { request_id, error } => {
            let Some(label) = state.pending.remove(&request_id) else {
                return;
            };
            if error == "success" || error.is_empty() {
                return;
            }
            if label == "loadfile" {
                emit(PlayerEvent::Error(format!(
                    "mpv rejected loadfile ({error})"
                )));
            } else {
                tracing::warn!(command = %label, error = %error, "mpv command failed");
            }
        }
        // Script messages are a video-overlay concern ([`super::video`]); the audio
        // engine has no keys to press.
        MpvIncoming::ClientMessage { .. } | MpvIncoming::Other => {}
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
        let mut state = DispatchState::default();

        dispatch_incoming(
            r#"{"event":"property-change","id":5,"name":"metadata","data":{"icy-title":"Artist - Track"}}"#,
            &emit,
            &mut state,
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
        let mut state = DispatchState::default();

        for data in ["1.1", "1.4", "1.9", "2.0"] {
            let line =
                format!(r#"{{"event":"property-change","id":1,"name":"time-pos","data":{data}}}"#);
            dispatch_incoming(&line, &emit, &mut state);
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
        let mut state = DispatchState {
            last_sent_time_sec: Some(3),
            ..DispatchState::default()
        };

        dispatch_incoming(
            r#"{"event":"property-change","id":1,"name":"time-pos","data":null}"#,
            &emit,
            &mut state,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(state.last_sent_time_sec, Some(3));
    }

    #[test]
    fn cache_time_forwards_and_dedups_to_whole_seconds() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();

        for data in ["100.2", "100.7", "101.3"] {
            let line = format!(
                r#"{{"event":"property-change","id":6,"name":"demuxer-cache-time","data":{data}}}"#
            );
            dispatch_incoming(&line, &emit, &mut state);
        }

        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::CacheTime(Some(t))) if t == 100.2));
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::CacheTime(Some(t))) if t == 101.3));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn null_cache_time_reports_loss_once() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState {
            last_sent_cache_sec: Some(42),
            ..DispatchState::default()
        };

        let line = r#"{"event":"property-change","id":6,"name":"demuxer-cache-time","data":null}"#;
        // First null after a real value → the loss is reported and the dedup resets…
        dispatch_incoming(line, &emit, &mut state);
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::CacheTime(None))));
        assert_eq!(state.last_sent_cache_sec, None);
        // …and repeated nulls (a stream that never has a cache) stay silent.
        dispatch_incoming(line, &emit, &mut state);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn null_duration_reports_loss_once_after_a_real_value() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();

        let null_line = r#"{"event":"property-change","id":2,"name":"duration","data":null}"#;
        // A null before any real value (observe echo on an unloaded player) stays silent.
        dispatch_incoming(null_line, &emit, &mut state);
        assert!(rx.try_recv().is_err());

        dispatch_incoming(
            r#"{"event":"property-change","id":2,"name":"duration","data":180.5}"#,
            &emit,
            &mut state,
        );
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::Duration(Some(d))) if d == 180.5));

        // First null after a real value → the loss is forwarded once, then silence.
        dispatch_incoming(null_line, &emit, &mut state);
        assert!(matches!(rx.try_recv(), Ok(PlayerEvent::Duration(None))));
        dispatch_incoming(null_line, &emit, &mut state);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn end_file_stop_resets_dedup_state_without_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState {
            last_sent_time_sec: Some(30),
            last_sent_cache_sec: Some(42),
            duration_known: true,
            ..DispatchState::default()
        };

        // Externally-caused stop/quit/redirect must clear the per-file dedup state (or the
        // next load's first second is swallowed) while emitting nothing — our own Stop
        // command already drives the reducers' stop paths.
        for reason in ["stop", "quit", "redirect", "some-future-reason"] {
            let line = format!(r#"{{"event":"end-file","reason":"{reason}"}}"#);
            state.last_sent_time_sec = Some(30);
            dispatch_incoming(&line, &emit, &mut state);
            assert!(rx.try_recv().is_err(), "reason {reason} must emit nothing");
            assert_eq!(state.last_sent_time_sec, None);
            assert_eq!(state.last_sent_cache_sec, None);
            assert!(!state.duration_known);
        }
    }

    #[test]
    fn failed_loadfile_reply_emits_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();
        state.pending.insert(11, "loadfile".to_owned());

        dispatch_incoming(
            r#"{"error":"invalid parameter","request_id":11}"#,
            &emit,
            &mut state,
        );

        assert!(
            matches!(rx.try_recv(), Ok(PlayerEvent::Error(error)) if error.contains("loadfile"))
        );
        assert!(!state.pending.contains_key(&11));
    }

    #[test]
    fn failed_af_command_reply_emits_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();
        state.pending.insert(12, "af-command".to_owned());

        dispatch_incoming(
            r#"{"error":"invalid parameter","request_id":12}"#,
            &emit,
            &mut state,
        );

        assert!(rx.try_recv().is_err());
        assert!(!state.pending.contains_key(&12));
    }

    #[test]
    fn success_reply_emits_nothing_and_removes_pending() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();
        state.pending.insert(13, "seek".to_owned());

        dispatch_incoming(r#"{"error":"success","request_id":13}"#, &emit, &mut state);

        assert!(rx.try_recv().is_err());
        assert!(!state.pending.contains_key(&13));
    }

    #[test]
    fn unknown_reply_id_is_ignored() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emit: EventSink = std::sync::Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut state = DispatchState::default();

        dispatch_incoming(
            r#"{"error":"invalid parameter","request_id":99}"#,
            &emit,
            &mut state,
        );

        assert!(rx.try_recv().is_err());
        assert!(state.pending.is_empty());
    }
}
