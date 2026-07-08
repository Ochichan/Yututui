//! IPC client for the `v` video-overlay mpv window.
//!
//! Unlike the audio engine's actor ([`super::ipc::run_actor`]), this client observes only
//! the overlay properties YuTuTui needs: `eof-reached` for auto-continue under
//! `--keep-open=yes`, and `pause` so overlay-local pause changes can update status without
//! mutating the underlying audio engine. A user quit (window close) still arrives as
//! `end-file reason=quit`; YuTuTui-owned keys arrive as `script-message` events.

use tokio::io::BufReader;
use tokio::sync::mpsc::{self, Sender};
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
    /// The user pressed the overlay pause key.
    TogglePause,
    /// mpv reported the overlay pause property.
    Paused(bool),
    /// The user requested the app-owned close path from inside the overlay.
    Close,
    /// The user pressed the overlay fullscreen key.
    ToggleFullscreen,
    /// The user pressed the overlay mute key.
    ToggleMute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoKeyAction {
    TogglePause,
    Next,
    Prev,
    Close,
    ToggleFullscreen,
    ToggleMute,
}

impl VideoKeyAction {
    fn command(self) -> &'static str {
        match self {
            Self::TogglePause => "script-message ytt-video-toggle-pause",
            Self::Next => "script-message ytt-video-next",
            Self::Prev => "script-message ytt-video-prev",
            Self::Close => "script-message ytt-video-close",
            Self::ToggleFullscreen => "script-message ytt-video-toggle-fullscreen",
            Self::ToggleMute => "script-message ytt-video-toggle-mute",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoKeyBinding {
    pub key: String,
    pub action: VideoKeyAction,
}

impl VideoKeyBinding {
    pub fn new(key: String, action: VideoKeyAction) -> Self {
        Self { key, action }
    }
}

/// Commands the reducer sends to the overlay window.
pub enum VideoCmd {
    /// `loadfile <url> replace` into the live window, then unpause (keep-open leaves
    /// mpv paused on the ended frame, and `pause` is sticky across loads).
    Load(String),
    CyclePause,
    CycleFullscreen,
    CycleMute,
}

/// Connect the IPC client for one overlay window and return its command sender
/// immediately; the connection (with retry) and the read loop run on a spawned task.
/// A connect failure is logged and the task ends silently — the overlay then degrades
/// to the pre-IPC fire-and-forget behavior rather than falsely reporting a close.
pub fn connect<F>(
    ipc_path: String,
    generation: u64,
    bindings: Vec<VideoKeyBinding>,
    emit: F,
) -> Sender<VideoCmd>
where
    F: Fn(u64, VideoEvent) + Send + Sync + 'static,
{
    let (tx, rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::VIDEO_CMD_QUEUE);
    tokio::spawn(async move {
        let conn = match connect_retry(&ipc_path).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "video overlay IPC connect failed");
                return;
            }
        };
        run(conn, rx, generation, bindings, &emit).await;
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
    mut cmd_rx: mpsc::Receiver<VideoCmd>,
    generation: u64,
    bindings: Vec<VideoKeyBinding>,
    emit: &F,
) where
    F: Fn(u64, VideoEvent),
{
    if let Err(e) = write_json(&conn, &proto::cmd_observe(1, "eof-reached")).await {
        tracing::warn!(error = %e, "failed to observe eof-reached on the video overlay");
    }
    if let Err(e) = write_json(&conn, &proto::cmd_observe(2, "pause")).await {
        tracing::warn!(error = %e, "failed to observe pause on the video overlay");
    }
    // Route configured YuTuTui overlay keys plus fixed mpv-compatibility aliases
    // through the app. On an mpv too old for `keybind`, errors are harmless and the
    // affected keys keep their mpv defaults.
    let mut setup_request_id: u64 = 10;
    for (key, msg) in keybind_specs(&bindings) {
        if let Err(e) = write_json(&conn, &proto::cmd_keybind(key, msg, setup_request_id)).await {
            tracing::warn!(error = %e, key, "failed to bind a video overlay key");
        }
        setup_request_id += 1;
    }

    let mut reader = BufReader::new(&conn);
    let mut line: Vec<u8> = Vec::new();
    let mut request_id: u64 = setup_request_id + 10;
    // One Eof per ended file: `eof-reached` re-arms the latch when it flips back to
    // false after a load, and the latch also swallows a duplicate `end-file reason=eof`.
    let mut eof_latched = false;

    loop {
        line.clear();
        tokio::select! {
            // Bounded read (shared cap with the audio actor): a broken or hostile overlay
            // endpoint can't grow `line` without limit before a newline arrives.
            read = crate::util::io::read_bounded_line(&mut reader, &mut line, super::ipc::MPV_IPC_MAX_LINE) => match read {
                Ok(crate::util::io::BoundedLine::Eof) | Err(_) => break, // mpv closed / transport error
                Ok(crate::util::io::BoundedLine::TooLarge) => {
                    tracing::warn!("mpv video overlay IPC line exceeded cap; closing");
                    break;
                }
                Ok(crate::util::io::BoundedLine::Line) => {
                    let text = String::from_utf8_lossy(&line);
                    if let Some(event) = interpret(&text, &mut eof_latched) {
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
                Some(VideoCmd::CyclePause) => {
                    request_id += 1;
                    if let Err(e) = write_json(&conn, &proto::cmd_cycle("pause", request_id)).await {
                        tracing::warn!(error = %e, "failed to toggle video overlay pause");
                    }
                }
                Some(VideoCmd::CycleFullscreen) => {
                    request_id += 1;
                    if let Err(e) = write_json(&conn, &proto::cmd_cycle("fullscreen", request_id)).await {
                        tracing::warn!(error = %e, "failed to toggle video overlay fullscreen");
                    }
                }
                Some(VideoCmd::CycleMute) => {
                    request_id += 1;
                    if let Err(e) = write_json(&conn, &proto::cmd_cycle("mute", request_id)).await {
                        tracing::warn!(error = %e, "failed to toggle video overlay mute");
                    }
                }
            },
        }
    }
}

fn keybind_specs(bindings: &[VideoKeyBinding]) -> Vec<(&str, &'static str)> {
    let mut out: Vec<(&str, &'static str)> = bindings
        .iter()
        .map(|binding| (binding.key.as_str(), binding.action.command()))
        .collect();
    out.extend([
        (">", VideoKeyAction::Next.command()),
        ("<", VideoKeyAction::Prev.command()),
        ("p", VideoKeyAction::TogglePause.command()),
    ]);
    out
}

/// Translate one mpv line into an overlay event. `eof_latched` dedups the natural-end
/// signal, which can arrive both as the `eof-reached` property and (without keep-open)
/// as `end-file reason=eof`.
fn interpret(line: &str, eof_latched: &mut bool) -> Option<VideoEvent> {
    match proto::parse_line(line)? {
        MpvIncoming::PropertyChange {
            id: Some(1),
            name,
            value,
        } if name == "eof-reached" => match value.as_bool() {
            Some(true) if !*eof_latched => {
                *eof_latched = true;
                Some(VideoEvent::Eof)
            }
            Some(false) => {
                *eof_latched = false;
                None
            }
            _ => None,
        },
        MpvIncoming::PropertyChange {
            id: Some(2),
            name,
            value,
        } if name == "pause" => value.as_bool().map(VideoEvent::Paused),
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
            Some("ytt-video-toggle-pause") => Some(VideoEvent::TogglePause),
            Some("ytt-video-close") => Some(VideoEvent::Close),
            Some("ytt-video-toggle-fullscreen") => Some(VideoEvent::ToggleFullscreen),
            Some("ytt-video-toggle-mute") => Some(VideoEvent::ToggleMute),
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
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-toggle-pause"]}"#,
                &mut latched
            ),
            Some(VideoEvent::TogglePause)
        ));
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-close"]}"#,
                &mut latched
            ),
            Some(VideoEvent::Close)
        ));
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-toggle-fullscreen"]}"#,
                &mut latched
            ),
            Some(VideoEvent::ToggleFullscreen)
        ));
        assert!(matches!(
            interp(
                r#"{"event":"client-message","args":["ytt-video-toggle-mute"]}"#,
                &mut latched
            ),
            Some(VideoEvent::ToggleMute)
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
    fn pause_property_changes_map_to_pause_events() {
        let mut latched = false;
        assert!(matches!(
            interp(
                r#"{"event":"property-change","id":2,"name":"pause","data":true}"#,
                &mut latched
            ),
            Some(VideoEvent::Paused(true))
        ));
        assert!(matches!(
            interp(
                r#"{"event":"property-change","id":2,"name":"pause","data":false}"#,
                &mut latched
            ),
            Some(VideoEvent::Paused(false))
        ));
    }

    #[test]
    fn keybind_specs_include_remapped_keys_and_fixed_aliases() {
        let bindings = vec![
            VideoKeyBinding::new("SPACE".to_owned(), VideoKeyAction::TogglePause),
            VideoKeyBinding::new(".".to_owned(), VideoKeyAction::Next),
            VideoKeyBinding::new(",".to_owned(), VideoKeyAction::Prev),
        ];
        let specs = keybind_specs(&bindings);
        assert!(specs.contains(&("SPACE", "script-message ytt-video-toggle-pause")));
        assert!(specs.contains(&(".", "script-message ytt-video-next")));
        assert!(specs.contains(&(",", "script-message ytt-video-prev")));
        assert!(specs.contains(&(">", "script-message ytt-video-next")));
        assert!(specs.contains(&("<", "script-message ytt-video-prev")));
        assert!(specs.contains(&("p", "script-message ytt-video-toggle-pause")));
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let mut latched = false;
        for line in [
            r#"{"event":"property-change","id":3,"name":"mute","data":true}"#,
            r#"{"error":"success","request_id":11}"#,
            "",
            "garbage",
        ] {
            assert!(interp(line, &mut latched).is_none());
        }
    }
}
