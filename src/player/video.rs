//! IPC client for the `v` video-overlay mpv window.
//!
//! Unlike the audio engine's actor ([`super::ipc::run_actor`]), this client observes a
//! single property — `eof-reached` — because the overlay runs with `--keep-open=yes`:
//! at a natural end mpv pauses on the last frame (no `end-file` fires) and flips
//! `eof-reached` to `true`, leaving a live window we can `loadfile` the next video into.
//! A user quit (`q` / window close) still arrives as `end-file reason=quit`.

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::{Duration, sleep};

use super::ipc::{connect_retry, write_json};
use super::proto::{self, MpvIncoming};

/// Events from the overlay mpv. Every event is delivered tagged with the spawn
/// generation the connection was made for, so the reducer can drop events from a window
/// it already closed or respawned.
pub enum VideoEvent {
    /// The video played to its natural end (`eof-reached` flipped true under
    /// `--keep-open`; `end-file reason=eof` is kept as a fallback for the same meaning).
    Eof,
    /// mpv could not play the file (`end-file reason=error`); carries mpv's `file_error`
    /// detail when it gave one (may be empty).
    Failed(String),
    /// The user quit the overlay (`q`, window close button). Deliberately *not* emitted
    /// for `reason=stop`: our own `loadfile … replace` ends the previous file with
    /// `stop`, which must not read as "user closed the window".
    Quit,
    /// The IPC connection dropped (mpv exited — after a quit, a crash, or a kill).
    Closed,
    /// The user pressed the next-track key (`>`) inside the overlay window.
    Next,
    /// The user pressed the previous-track key (`<`) inside the overlay window.
    Prev,
}

/// Commands the reducer sends to the overlay window.
pub enum VideoCmd {
    /// `loadfile <url> replace` into the live window, then unpause (keep-open leaves
    /// mpv paused on the ended frame, and `pause` is sticky across loads).
    Load(String),
}

/// Connect the IPC client for one overlay window and return its command sender
/// immediately; the connection (with retry) and the read loop run on a spawned task.
/// A connect failure is logged and the task ends silently — the overlay then degrades
/// to the pre-IPC fire-and-forget behavior rather than falsely reporting a close.
pub fn connect<F>(ipc_path: String, generation: u64, emit: F) -> UnboundedSender<VideoCmd>
where
    F: Fn(u64, VideoEvent) + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let conn = match connect_retry(&ipc_path).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "video overlay IPC connect failed");
                return;
            }
        };
        run(conn, rx, generation, &emit).await;
        // Give mpv a beat to finish exiting so the reducer's `try_wait` probe sees a
        // dead process (a live-window IPC hiccup then reads as "still open" instead).
        sleep(Duration::from_millis(300)).await;
        emit(generation, VideoEvent::Closed);
    });
    tx
}

/// The connected loop: observe `eof-reached`, forward interpreted events, write
/// `loadfile` commands. Returns when mpv closes the connection or the sender drops.
async fn run<F>(
    conn: interprocess::local_socket::tokio::Stream,
    mut cmd_rx: mpsc::UnboundedReceiver<VideoCmd>,
    generation: u64,
    emit: &F,
) where
    F: Fn(u64, VideoEvent),
{
    if let Err(e) = write_json(&conn, &proto::cmd_observe(1, "eof-reached")).await {
        tracing::warn!(error = %e, "failed to observe eof-reached on the video overlay");
    }
    // Route mpv's own playlist-next/prev keys to the app queue. The overlay always
    // plays a single-entry playlist, so the default bindings are dead ends anyway; on
    // an mpv too old for `keybind` the command errors harmlessly and the keys stay dead.
    for (id, key, msg) in [
        (2u64, ">", "script-message ytt-video-next"),
        (3, "<", "script-message ytt-video-prev"),
    ] {
        if let Err(e) = write_json(&conn, &proto::cmd_keybind(key, msg, id)).await {
            tracing::warn!(error = %e, key, "failed to bind a video overlay key");
        }
    }

    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    let mut request_id: u64 = 10;
    // One Eof per ended file: `eof-reached` re-arms the latch when it flips back to
    // false after a load, and the latch also swallows a duplicate `end-file reason=eof`.
    let mut eof_latched = false;

    loop {
        line.clear();
        tokio::select! {
            read = reader.read_line(&mut line) => match read {
                Ok(0) | Err(_) => break, // mpv closed the connection
                Ok(_) => {
                    if let Some(event) = interpret(&line, &mut eof_latched) {
                        emit(generation, event);
                    }
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                None => break, // sender dropped: the app is shutting down
                Some(VideoCmd::Load(url)) => {
                    request_id += 1;
                    let load = proto::cmd_loadfile(&url, "replace", request_id);
                    request_id += 1;
                    let unpause = proto::cmd_set_property(
                        "pause",
                        &serde_json::Value::Bool(false),
                        request_id,
                    );
                    for json in [load, unpause] {
                        if let Err(e) = write_json(&conn, &json).await {
                            tracing::warn!(error = %e, "failed to write video overlay command");
                        }
                    }
                }
            },
        }
    }
}

/// Translate one mpv line into an overlay event. `eof_latched` dedups the natural-end
/// signal, which can arrive both as the `eof-reached` property and (without keep-open)
/// as `end-file reason=eof`.
fn interpret(line: &str, eof_latched: &mut bool) -> Option<VideoEvent> {
    match proto::parse_line(line)? {
        MpvIncoming::PropertyChange { name, value } if name == "eof-reached" => {
            match value.as_bool() {
                Some(true) if !*eof_latched => {
                    *eof_latched = true;
                    Some(VideoEvent::Eof)
                }
                Some(false) => {
                    *eof_latched = false;
                    None
                }
                _ => None,
            }
        }
        MpvIncoming::EndFile { reason, file_error } => match reason.as_str() {
            "eof" if !*eof_latched => {
                *eof_latched = true;
                Some(VideoEvent::Eof)
            }
            "error" => Some(VideoEvent::Failed(file_error.unwrap_or_default())),
            "quit" => Some(VideoEvent::Quit),
            // `stop` is our own `loadfile … replace` ending the previous file (and
            // `redirect` is playlist bookkeeping) — neither means the window closed.
            _ => None,
        },
        MpvIncoming::ClientMessage { args } => match args.first().map(String::as_str) {
            Some("ytt-video-next") => Some(VideoEvent::Next),
            Some("ytt-video-prev") => Some(VideoEvent::Prev),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp(line: &str, latched: &mut bool) -> Option<VideoEvent> {
        interpret(line, latched)
    }

    #[test]
    fn eof_reached_true_emits_once_until_rearmed() {
        let mut latched = false;
        let line = r#"{"event":"property-change","id":1,"name":"eof-reached","data":true}"#;
        assert!(matches!(interp(line, &mut latched), Some(VideoEvent::Eof)));
        // A duplicate true (or a redundant end-file eof) is swallowed by the latch.
        assert!(interp(line, &mut latched).is_none());
        assert!(interp(r#"{"event":"end-file","reason":"eof"}"#, &mut latched).is_none());
        // The property flipping false (next file loading) re-arms the latch.
        let false_line = r#"{"event":"property-change","id":1,"name":"eof-reached","data":false}"#;
        assert!(interp(false_line, &mut latched).is_none());
        assert!(matches!(interp(line, &mut latched), Some(VideoEvent::Eof)));
    }

    #[test]
    fn initial_observe_echo_is_ignored() {
        let mut latched = false;
        // mpv echoes the current value on observe: false at start, null before load.
        for data in ["false", "null"] {
            let line = format!(
                r#"{{"event":"property-change","id":1,"name":"eof-reached","data":{data}}}"#
            );
            assert!(interp(&line, &mut latched).is_none());
        }
    }

    #[test]
    fn end_file_reasons_map_to_events() {
        let mut latched = false;
        assert!(matches!(
            interp(r#"{"event":"end-file","reason":"quit"}"#, &mut latched),
            Some(VideoEvent::Quit)
        ));
        // Our own `loadfile … replace` ends the old file with `stop` — not a close.
        assert!(interp(r#"{"event":"end-file","reason":"stop"}"#, &mut latched).is_none());
        match interp(
            r#"{"event":"end-file","reason":"error","file_error":"Failed to open"}"#,
            &mut latched,
        ) {
            Some(VideoEvent::Failed(detail)) => assert_eq!(detail, "Failed to open"),
            _ => panic!("expected Failed"),
        }
        assert!(matches!(
            interp(r#"{"event":"end-file","reason":"eof"}"#, &mut latched),
            Some(VideoEvent::Eof)
        ));
    }

    #[test]
    fn client_messages_map_to_next_prev() {
        let mut latched = false;
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-next"]}"#,
                &mut latched
            ),
            Some(VideoEvent::Next)
        ));
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-prev"]}"#,
                &mut latched
            ),
            Some(VideoEvent::Prev)
        ));
        // Foreign script messages (user scripts run in the overlay too) are ignored.
        assert!(
            interp(
                r#"{"event":"client-message","args":["osc-visibility","auto"]}"#,
                &mut latched
            )
            .is_none()
        );
        assert!(interp(r#"{"event":"client-message"}"#, &mut latched).is_none());
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let mut latched = false;
        for line in [
            r#"{"event":"property-change","id":3,"name":"pause","data":true}"#,
            r#"{"error":"success","request_id":11}"#,
            "",
            "garbage",
        ] {
            assert!(interp(line, &mut latched).is_none());
        }
    }
}
