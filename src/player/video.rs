//! IPC client for the `v` video-overlay mpv window.
//!
//! Unlike the audio engine's actor ([`super::ipc::run_actor`]), this client observes only
//! the overlay properties YuTuTui needs: `eof-reached` for auto-continue under
//! `--keep-open=yes`, and `pause` so overlay-local pause changes can update status without
//! mutating the underlying audio engine. A user quit (window close) still arrives as
//! `end-file reason=quit`; YuTuTui-owned keys arrive as `script-message` events.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::BufReader;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{Duration, sleep};

use super::ipc::{connect_retry, write_json};
use super::proto::{self, MpvIncoming};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

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

/// Bounded command ingress for one video-overlay generation.
///
/// A full IPC lane spills into one bounded, ordered backlog drained by one task. Only
/// adjacent pending loads coalesce: toggles are ordering barriers because mpv state can
/// also change outside YuTuTui, so combining them would not be semantically safe.
pub struct VideoHandle {
    tx: Sender<VideoCmd>,
    pending: Arc<Mutex<VideoPending>>,
}

impl VideoHandle {
    fn new(tx: Sender<VideoCmd>) -> Self {
        let capacity = crate::util::backpressure::VIDEO_CMD_QUEUE
            .capacity()
            .expect("video command queue must be bounded");
        Self::with_pending_capacity(tx, capacity)
    }

    fn with_pending_capacity(tx: Sender<VideoCmd>, capacity: usize) -> Self {
        Self {
            tx,
            pending: Arc::new(Mutex::new(VideoPending::new(capacity))),
        }
    }

    pub fn send(&self, cmd: VideoCmd) -> DeliveryResult {
        let mut pending = lock_pending(&self.pending);
        if pending.closed || self.tx.is_closed() {
            pending.closed = true;
            pending.cmds.clear();
            return Err(DeliveryError::Closed);
        }
        if pending.drainer_running || !pending.cmds.is_empty() {
            return pending.push(cmd).map(pending_receipt);
        }

        match self.tx.try_send(cmd) {
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let runtime =
                    tokio::runtime::Handle::try_current().map_err(|_| DeliveryError::Busy)?;
                let coalesced = pending.push(cmd)?;
                pending.drainer_running = true;
                drop(pending);
                runtime.spawn(drain_video_queue(
                    self.tx.clone(),
                    Arc::clone(&self.pending),
                ));
                Ok(pending_receipt(coalesced))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                pending.closed = true;
                Err(DeliveryError::Closed)
            }
        }
    }
}

impl Drop for VideoHandle {
    fn drop(&mut self) {
        let mut pending = lock_pending(&self.pending);
        pending.closed = true;
        pending.cmds.clear();
    }
}

fn pending_receipt(coalesced: bool) -> DeliveryReceipt {
    if coalesced {
        DeliveryReceipt::Coalesced {
            replaced_existing: true,
            evicted_oldest: false,
        }
    } else {
        DeliveryReceipt::Deferred
    }
}

struct VideoPending {
    cmds: VecDeque<VideoCmd>,
    capacity: usize,
    drainer_running: bool,
    closed: bool,
}

impl VideoPending {
    fn new(capacity: usize) -> Self {
        Self {
            cmds: VecDeque::new(),
            capacity,
            drainer_running: false,
            closed: false,
        }
    }

    /// Returns whether an existing command was replaced.
    fn push(&mut self, cmd: VideoCmd) -> Result<bool, DeliveryError> {
        if let VideoCmd::Load(url) = cmd {
            if let Some(VideoCmd::Load(existing)) = self.cmds.back_mut() {
                *existing = url;
                return Ok(true);
            }
            return self.push_new(VideoCmd::Load(url));
        }
        self.push_new(cmd)
    }

    fn push_new(&mut self, cmd: VideoCmd) -> Result<bool, DeliveryError> {
        if self.cmds.len() >= self.capacity {
            return Err(DeliveryError::Busy);
        }
        self.cmds.push_back(cmd);
        Ok(false)
    }
}

fn lock_pending(pending: &Mutex<VideoPending>) -> MutexGuard<'_, VideoPending> {
    pending.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("video command backlog mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

async fn drain_video_queue(tx: Sender<VideoCmd>, pending: Arc<Mutex<VideoPending>>) {
    loop {
        let permit = match tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => {
                let mut pending = lock_pending(&pending);
                pending.cmds.clear();
                pending.drainer_running = false;
                pending.closed = true;
                return;
            }
        };
        let cmd = {
            let mut pending = lock_pending(&pending);
            if pending.closed {
                pending.cmds.clear();
            }
            match pending.cmds.pop_front() {
                Some(cmd) => Some(cmd),
                None => {
                    pending.drainer_running = false;
                    None
                }
            }
        };
        match cmd {
            Some(cmd) => permit.send(cmd),
            None => {
                drop(permit);
                return;
            }
        }
    }
}

#[derive(Debug)]
struct VideoIpcFailure {
    detail: String,
}

impl VideoIpcFailure {
    fn new(operation: &'static str, error: &std::io::Error) -> Self {
        let detail = crate::util::sanitize::sanitize_error_text(format!(
            "video overlay IPC {operation} failed: {error}"
        ));
        tracing::warn!(error = %detail, operation, "video overlay IPC operation failed");
        Self { detail }
    }
}

enum VideoTerminal {
    Closed,
    Failed(VideoIpcFailure),
    Quit,
}

impl VideoTerminal {
    fn waits_for_process_exit(&self) -> bool {
        matches!(self, Self::Closed)
    }

    fn into_event(self) -> VideoEvent {
        match self {
            Self::Closed => VideoEvent::Closed,
            Self::Failed(failure) => VideoEvent::Failed(failure.detail),
            Self::Quit => VideoEvent::Quit,
        }
    }
}

async fn emit_terminal<F>(generation: u64, terminal: VideoTerminal, emit: &F)
where
    F: Fn(u64, VideoEvent),
{
    if terminal.waits_for_process_exit() {
        // Give mpv a beat to finish exiting so the reducer's `try_wait` probe sees a
        // dead process (a live-window IPC hiccup then reads as "still open" instead).
        sleep(Duration::from_millis(300)).await;
    }
    // Every connection task has exactly one terminal emission. In particular, a Failed
    // write never falls through to a second Closed event for the same generation.
    emit(generation, terminal.into_event());
}

async fn write_video_json(
    conn: &interprocess::local_socket::tokio::Stream,
    json: &str,
    operation: &'static str,
) -> Result<(), VideoIpcFailure> {
    write_json(conn, json)
        .await
        .map_err(|error| VideoIpcFailure::new(operation, &error))
}

/// Connect the IPC client for one overlay window and return its command sender
/// immediately; the connection (with retry) and the read loop run on a spawned task.
/// A connect, setup, or command-write failure emits one generation-tagged
/// [`VideoEvent::Failed`] and does not also emit [`VideoEvent::Closed`], so an accepted
/// overlay request cannot disappear.
pub fn connect<F>(
    ipc_path: String,
    generation: u64,
    bindings: Vec<VideoKeyBinding>,
    emit: F,
) -> VideoHandle
where
    F: Fn(u64, VideoEvent) + Send + Sync + 'static,
{
    let (tx, rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::VIDEO_CMD_QUEUE);
    tokio::spawn(async move {
        finish_connection(
            connect_retry(&ipc_path).await,
            rx,
            generation,
            bindings,
            emit,
        )
        .await;
    });
    VideoHandle::new(tx)
}

async fn finish_connection<F>(
    connection: std::io::Result<interprocess::local_socket::tokio::Stream>,
    rx: mpsc::Receiver<VideoCmd>,
    generation: u64,
    bindings: Vec<VideoKeyBinding>,
    emit: F,
) where
    F: Fn(u64, VideoEvent),
{
    let terminal = match connection {
        Ok(conn) => match run(conn, rx, generation, bindings, &emit).await {
            Ok(terminal) => terminal,
            Err(failure) => VideoTerminal::Failed(failure),
        },
        Err(error) => VideoTerminal::Failed(VideoIpcFailure::new("connection", &error)),
    };
    emit_terminal(generation, terminal, &emit).await;
}

/// The connected loop: observe `eof-reached`, forward interpreted events, write
/// `loadfile` commands. Returns when mpv closes the connection or the sender drops.
async fn run<F>(
    conn: interprocess::local_socket::tokio::Stream,
    mut cmd_rx: mpsc::Receiver<VideoCmd>,
    generation: u64,
    bindings: Vec<VideoKeyBinding>,
    emit: &F,
) -> Result<VideoTerminal, VideoIpcFailure>
where
    F: Fn(u64, VideoEvent),
{
    write_video_json(
        &conn,
        &proto::cmd_observe(1, "eof-reached"),
        "eof-reached observer setup",
    )
    .await?;
    write_video_json(
        &conn,
        &proto::cmd_observe(2, "pause"),
        "pause observer setup",
    )
    .await?;
    // Route configured YuTuTui overlay keys plus fixed mpv-compatibility aliases
    // through the app. On an mpv too old for `keybind`, errors are harmless and the
    // affected keys keep their mpv defaults.
    let mut setup_request_id: u64 = 10;
    for (key, msg) in keybind_specs(&bindings) {
        write_video_json(
            &conn,
            &proto::cmd_keybind(key, msg, setup_request_id),
            "key binding setup",
        )
        .await?;
        setup_request_id += 1;
    }

    let mut reader = BufReader::new(&conn);
    let mut line: Vec<u8> = Vec::new();
    let mut request_id: u64 = setup_request_id + 10;
    // One Eof per ended file: `eof-reached` re-arms the latch when it flips back to
    // false after a load, and the latch also swallows a duplicate `end-file reason=eof`.
    let mut eof_latched = false;

    loop {
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
                        match event {
                            VideoEvent::Failed(detail) => {
                                return Ok(VideoTerminal::Failed(VideoIpcFailure {
                                    detail: crate::util::sanitize::sanitize_error_text(detail),
                                }));
                            }
                            VideoEvent::Quit => return Ok(VideoTerminal::Quit),
                            event => emit(generation, event),
                        }
                    }
                    // A command can win the select while the newline-framed read is pending.
                    // Retain that prefix and clear only after the complete frame is consumed.
                    line.clear();
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
                    write_video_json(&conn, &load, "load command write").await?;
                    write_video_json(&conn, &unpause, "unpause command write").await?;
                }
                Some(VideoCmd::CyclePause) => {
                    request_id += 1;
                    write_video_json(
                        &conn,
                        &proto::cmd_cycle("pause", request_id),
                        "pause toggle command write",
                    )
                    .await?;
                }
                Some(VideoCmd::CycleFullscreen) => {
                    request_id += 1;
                    write_video_json(
                        &conn,
                        &proto::cmd_cycle("fullscreen", request_id),
                        "fullscreen toggle command write",
                    )
                    .await?;
                }
                Some(VideoCmd::CycleMute) => {
                    request_id += 1;
                    write_video_json(
                        &conn,
                        &proto::cmd_cycle("mute", request_id),
                        "mute toggle command write",
                    )
                    .await?;
                }
            },
        }
    }

    Ok(VideoTerminal::Closed)
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
        MpvIncoming::EndFile {
            reason, file_error, ..
        } => match reason.as_str() {
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

    #[tokio::test]
    async fn full_video_lane_coalesces_only_adjacent_pending_loads() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(VideoCmd::CycleMute)
            .expect("occupy the direct lane");
        let handle = VideoHandle::with_pending_capacity(tx, 4);

        assert_eq!(
            handle.send(VideoCmd::Load("old".to_owned())),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.send(VideoCmd::Load("new".to_owned())),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );

        assert!(matches!(rx.recv().await, Some(VideoCmd::CycleMute)));
        assert!(matches!(
            rx.recv().await,
            Some(VideoCmd::Load(url)) if url == "new"
        ));
    }

    #[tokio::test]
    async fn video_toggles_are_ordering_barriers_for_load_coalescing() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(VideoCmd::CycleMute)
            .expect("occupy the direct lane");
        let handle = VideoHandle::with_pending_capacity(tx, 4);

        assert_eq!(
            handle.send(VideoCmd::Load("old".to_owned())),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.send(VideoCmd::CyclePause),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.send(VideoCmd::Load("new".to_owned())),
            Ok(DeliveryReceipt::Deferred)
        );

        assert!(matches!(rx.recv().await, Some(VideoCmd::CycleMute)));
        assert!(matches!(
            rx.recv().await,
            Some(VideoCmd::Load(url)) if url == "old"
        ));
        assert!(matches!(rx.recv().await, Some(VideoCmd::CyclePause)));
        assert!(matches!(
            rx.recv().await,
            Some(VideoCmd::Load(url)) if url == "new"
        ));
    }

    #[tokio::test]
    async fn video_pending_backlog_reports_busy_at_its_bound() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(VideoCmd::CycleMute)
            .expect("occupy the direct lane");
        let handle = VideoHandle::with_pending_capacity(tx, 1);

        assert_eq!(
            handle.send(VideoCmd::CyclePause),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.send(VideoCmd::CycleFullscreen),
            Err(DeliveryError::Busy)
        );
    }

    #[test]
    fn closed_video_lane_reports_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = VideoHandle::with_pending_capacity(tx, 1);

        assert_eq!(
            handle.send(VideoCmd::CyclePause),
            Err(DeliveryError::Closed)
        );
    }

    #[test]
    fn full_video_lane_without_runtime_reports_busy() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(VideoCmd::CycleMute)
            .expect("occupy the direct lane");
        let handle = VideoHandle::with_pending_capacity(tx, 1);

        assert_eq!(handle.send(VideoCmd::CyclePause), Err(DeliveryError::Busy));
    }

    #[tokio::test]
    async fn connect_failure_emits_one_sanitized_failed_event_for_its_generation() {
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let secret = "short-secret";

        finish_connection(
            Err(std::io::Error::other(format!(
                "refused access_token={secret}"
            ))),
            cmd_rx,
            73,
            Vec::new(),
            move |generation, event| {
                captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((generation, event));
            },
        )
        .await;

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "connect failure has one terminal event");
        let (generation, event) = &events[0];
        assert_eq!(*generation, 73);
        let VideoEvent::Failed(detail) = event else {
            panic!("connect failure must not be reported as Closed");
        };
        assert!(detail.contains("video overlay IPC connection failed"));
        assert!(detail.contains("access_token=<redacted>"));
        assert!(!detail.contains(secret));
    }

    #[tokio::test]
    async fn write_failure_emits_one_sanitized_failed_event_without_closed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let secret = "write-secret";
        let emit = move |generation, event| {
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((generation, event));
        };
        let failure = VideoIpcFailure::new(
            "load command write",
            &std::io::Error::other(format!("broken access_token={secret}")),
        );

        emit_terminal(91, VideoTerminal::Failed(failure), &emit).await;

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "write failure has one terminal event");
        let (generation, event) = &events[0];
        assert_eq!(*generation, 91);
        let VideoEvent::Failed(detail) = event else {
            panic!("write failure must not fall through to Closed");
        };
        assert!(detail.contains("video overlay IPC load command write failed"));
        assert!(detail.contains("access_token=<redacted>"));
        assert!(!detail.contains(secret));
    }
}
