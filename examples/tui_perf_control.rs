//! Out-of-band playback controller for the TUI performance harness.
//!
//! User actions go through the existing authenticated remote protocol. A second,
//! read-only mpv IPC connection observes `playback-restart`, `paused-for-cache`, and
//! `time-pos` so latency and buffering are measured without injecting terminal keys.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use yututui::remote::proto::{RemoteCommand, StatusSnapshot};

const SCHEMA: &str = "ytt.tui-perf.control.v1";
const IPC_LINE_CAP: usize = 1024 * 1024;
const MIN_RESUME_PROGRESS_S: f64 = 0.01;

#[derive(Debug)]
enum LoadAction {
    None,
    ResumeSession,
    Play(String),
}

#[derive(Debug)]
struct Args {
    output: PathBuf,
    wait: Duration,
    observe: Duration,
    close_grace: Duration,
    load: LoadAction,
    seeks: Vec<f64>,
    pause_hold: Option<Duration>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut output = None;
        let mut wait_secs = 30.0;
        let mut observe_secs = 0.0;
        let mut close_grace_secs = 0.0;
        let mut load = LoadAction::None;
        let mut seeks = Vec::new();
        let mut pause_hold = None;
        let mut raw = std::env::args().skip(1);
        while let Some(arg) = raw.next() {
            let next = |name: &str, it: &mut std::iter::Skip<std::env::Args>| {
                it.next().ok_or_else(|| format!("{name} requires a value"))
            };
            match arg.as_str() {
                "--output" => output = Some(PathBuf::from(next("--output", &mut raw)?)),
                "--wait-secs" => {
                    wait_secs = next("--wait-secs", &mut raw)?
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .ok_or_else(|| "--wait-secs must be a positive number".to_string())?;
                }
                "--observe-secs" => {
                    observe_secs = next("--observe-secs", &mut raw)?
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .ok_or_else(|| {
                            "--observe-secs must be a non-negative number".to_string()
                        })?;
                }
                "--close-grace-secs" => {
                    close_grace_secs = next("--close-grace-secs", &mut raw)?
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .ok_or_else(|| {
                            "--close-grace-secs must be a non-negative number".to_string()
                        })?;
                }
                "--load" => {
                    load = match next("--load", &mut raw)?.as_str() {
                        "none" => LoadAction::None,
                        "resume-session" => LoadAction::ResumeSession,
                        other => {
                            return Err(format!(
                                "unsupported --load value `{other}` (expected none|resume-session)"
                            ));
                        }
                    };
                }
                "--play-query" => load = LoadAction::Play(next("--play-query", &mut raw)?),
                "--seeks" => {
                    seeks = parse_seeks(&next("--seeks", &mut raw)?)?;
                }
                "--pause-hold-ms" => {
                    let millis = next("--pause-hold-ms", &mut raw)?
                        .parse::<u64>()
                        .map_err(|_| "--pause-hold-ms must be an integer".to_string())?;
                    pause_hold = Some(Duration::from_millis(millis));
                }
                "--no-pause" => pause_hold = None,
                "-h" | "--help" => return Err(usage().to_string()),
                other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
            }
        }
        Ok(Self {
            output: output.ok_or_else(|| "--output is required".to_string())?,
            wait: Duration::from_secs_f64(wait_secs),
            observe: Duration::from_secs_f64(observe_secs),
            close_grace: Duration::from_secs_f64(close_grace_secs),
            load,
            seeks,
            pause_hold,
        })
    }
}

fn usage() -> &'static str {
    "Usage: tui_perf_control --output FILE [options]\n\
     Options:\n\
       --wait-secs N                 Per-operation timeout (default 30)\n\
       --observe-secs N              Drain mpv telemetry for the full run (default 0)\n\
       --close-grace-secs N          Extra final wait for sampler-owned mpv close (default 0)\n\
       --load none|resume-session    Optional deterministic session load\n\
       --play-query QUERY            Ask the owner to search and play QUERY\n\
       --seeks S1,S2,...             Absolute seek targets in seconds\n\
       --pause-hold-ms N             Explicit pause duration before resume\n\
       --no-pause                    Explicitly keep playback steady (default)"
}

fn parse_seeks(raw: &str) -> Result<Vec<f64>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| format!("invalid seek target `{part}`"))
        })
        .collect()
}

fn scheduled_offset(window: Duration, ordinal: usize, total_actions: usize) -> Duration {
    if window.is_zero() || ordinal == 0 || total_actions == 0 {
        return Duration::ZERO;
    }
    debug_assert!(ordinal <= total_actions);
    window.mul_f64(ordinal as f64 / (total_actions + 1) as f64)
}

fn scheduled_action_count(seeks: usize, pause_hold: Option<Duration>) -> usize {
    seeks + usize::from(pause_hold.is_some())
}

#[derive(Debug)]
struct IpcEvent {
    observed_at: Instant,
    value: Value,
}

struct Telemetry {
    origin: Instant,
    buffering_since: Option<Instant>,
    buffering_events: u64,
    buffering_total: Duration,
    last_time_pos_s: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainEnd {
    Deadline,
    ObserverClosed,
}

impl DrainEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::ObserverClosed => "mpv_ipc_closed",
        }
    }
}

impl Telemetry {
    fn new(origin: Instant) -> Self {
        Self {
            origin,
            buffering_since: None,
            buffering_events: 0,
            buffering_total: Duration::ZERO,
            last_time_pos_s: None,
        }
    }

    fn record(&mut self, event: &IpcEvent, out: &mut BufWriter<File>) -> Result<(), String> {
        if let Some(position) = time_pos_seconds(&event.value) {
            self.last_time_pos_s = Some(position);
        }
        if event.value.get("event").and_then(Value::as_str) == Some("property-change")
            && event.value.get("name").and_then(Value::as_str) == Some("paused-for-cache")
            && let Some(paused) = event.value.get("data").and_then(Value::as_bool)
        {
            if paused && self.buffering_since.is_none() {
                self.buffering_since = Some(event.observed_at);
                self.buffering_events += 1;
            } else if !paused && let Some(started) = self.buffering_since.take() {
                self.buffering_total += event.observed_at.saturating_duration_since(started);
            }
        }
        write_ndjson(
            out,
            &json!({
                "schema": SCHEMA,
                "kind": "mpv_event",
                "elapsed_ms": event.observed_at.saturating_duration_since(self.origin).as_millis(),
                "event": event.value,
            }),
        )
    }

    fn finish(&mut self, now: Instant) {
        if let Some(started) = self.buffering_since.take() {
            self.buffering_total += now.saturating_duration_since(started);
        }
    }
}

fn time_pos_seconds(value: &Value) -> Option<f64> {
    (value.get("event").and_then(Value::as_str) == Some("property-change")
        && value.get("name").and_then(Value::as_str) == Some("time-pos"))
    .then(|| value.get("data").and_then(Value::as_f64))
    .flatten()
}

fn proves_resume_progress(value: &Value, paused_time_pos_s: f64) -> bool {
    time_pos_seconds(value)
        .is_some_and(|position| position >= paused_time_pos_s + MIN_RESUME_PROGRESS_S)
}

fn drain_pending_mpv(
    rx: &mut mpsc::UnboundedReceiver<IpcEvent>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    while let Ok(event) = rx.try_recv() {
        telemetry.record(&event, out)?;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let code = match Args::parse() {
        Ok(args) => match run(args).await {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("tui_perf_control: {message}");
                2
            }
        },
        Err(message) => {
            eprintln!("tui_perf_control: {message}");
            2
        }
    };
    std::process::exit(code);
}

async fn run(args: Args) -> Result<(), String> {
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }
    let mut out = BufWriter::new(
        File::create(&args.output).map_err(|e| format!("create {}: {e}", args.output.display()))?,
    );
    let origin = Instant::now();
    let instance = wait_for_instance(args.wait).await?;
    let (mpv_pid, mpv_endpoint) = discover_mpv(instance.app_pid, args.wait).await?;
    let stream = connect_mpv(&mpv_endpoint, args.wait).await?;
    subscribe(&stream).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_mpv(stream, tx));
    let mut telemetry = Telemetry::new(origin);

    write_ndjson(
        &mut out,
        &json!({
            "schema": SCHEMA,
            "kind": "header",
            "owner_pid": instance.app_pid,
            "owner_mode": instance.mode,
            "mpv_pid": mpv_pid,
            "mpv_endpoint": mpv_endpoint,
            "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "pause_policy": if args.pause_hold.is_some() { "pause-resume" } else { "none" },
            "pause_hold_ms": args.pause_hold.map(|hold| hold.as_millis()),
        }),
    )?;

    match args.load {
        LoadAction::None => {
            let started = Instant::now();
            let status = wait_for_status(args.wait, |status| status.title.is_some()).await?;
            operation(
                &mut out,
                "ready",
                started.elapsed(),
                json!({"title": status.title}),
            )?;
        }
        LoadAction::ResumeSession => {
            perform_load(
                RemoteCommand::ResumeSession,
                "resume_session",
                args.wait,
                &mut rx,
                &mut telemetry,
                &mut out,
            )
            .await?;
        }
        LoadAction::Play(query) => {
            perform_load(
                RemoteCommand::Play { query },
                "play_query",
                args.wait,
                &mut rx,
                &mut telemetry,
                &mut out,
            )
            .await?;
        }
    }

    // Seek through the app's public control lane so position_epoch and all owner invariants run.
    let scheduled_actions = scheduled_action_count(args.seeks.len(), args.pause_hold);
    for (index, target_s) in args.seeks.into_iter().enumerate() {
        if scheduled_actions > 0 && !args.observe.is_zero() {
            let offset = scheduled_offset(args.observe, index + 1, scheduled_actions);
            drain_to_action(origin + offset, &mut rx, &mut telemetry, &mut out).await?;
        }
        let started = Instant::now();
        send_remote(RemoteCommand::SeekTo {
            ms: (target_s * 1_000.0).round() as u64,
        })
        .await?;
        wait_for_mpv(args.wait, &mut rx, &mut telemetry, &mut out, |value| {
            value.get("event").and_then(Value::as_str) == Some("playback-restart")
        })
        .await?;
        operation(
            &mut out,
            "seek",
            started.elapsed(),
            json!({"target_s": target_s}),
        )?;
    }

    if let Some(hold) = args.pause_hold {
        if scheduled_actions > 0 && !args.observe.is_zero() {
            let offset = scheduled_offset(args.observe, scheduled_actions, scheduled_actions);
            drain_to_action(origin + offset, &mut rx, &mut telemetry, &mut out).await?;
        }
        ensure_playing(args.wait).await?;
        let pause_started = Instant::now();
        send_remote(RemoteCommand::TogglePause).await?;
        wait_for_status(args.wait, |status| status.paused).await?;
        operation(&mut out, "pause", pause_started.elapsed(), json!({}))?;
        sleep(hold).await;

        // mpv 0.40 does not emit playback-restart for a plain unpause. Drain every event already
        // observed while paused, then require decoder time to move beyond the paused boundary.
        // The small threshold rejects sub-millisecond property jitter around that boundary.
        drain_pending_mpv(&mut rx, &mut telemetry, &mut out)?;
        let paused_time_pos_s = telemetry
            .last_time_pos_s
            .ok_or_else(|| "mpv did not publish time-pos before resume".to_string())?;
        let resume_started = Instant::now();
        send_remote(RemoteCommand::TogglePause).await?;
        wait_for_status(args.wait, |status| !status.paused).await?;
        wait_for_mpv(args.wait, &mut rx, &mut telemetry, &mut out, |value| {
            proves_resume_progress(value, paused_time_pos_s)
        })
        .await?;
        operation(
            &mut out,
            "resume",
            resume_started.elapsed(),
            json!({
                "pause_hold_ms": hold.as_millis(),
                "paused_time_pos_s": paused_time_pos_s,
            }),
        )?;
    }

    // Operations deliberately occupy only deterministic points in the observation window.
    // Continue draining until the full warmup+sample duration has elapsed so a late cache stall
    // cannot disappear from the no-added-rebuffer gate merely because the last seek finished.
    let observation_end = drain_until(
        origin + args.observe + args.close_grace,
        &mut rx,
        &mut telemetry,
        &mut out,
    )
    .await?;

    while let Ok(event) = rx.try_recv() {
        telemetry.record(&event, &mut out)?;
    }
    telemetry.finish(Instant::now());
    write_ndjson(
        &mut out,
        &json!({
            "schema": SCHEMA,
            "kind": "summary",
            "buffering_events": telemetry.buffering_events,
            "buffering_ms": telemetry.buffering_total.as_millis(),
            "observation_end": observation_end.as_str(),
        }),
    )?;
    out.flush().map_err(|e| format!("flush output: {e}"))
}

async fn drain_until(
    deadline: Instant,
    rx: &mut mpsc::UnboundedReceiver<IpcEvent>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<DrainEnd, String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(DrainEnd::Deadline);
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => telemetry.record(&event, out)?,
            Ok(None) => return Ok(DrainEnd::ObserverClosed),
            Err(_) => return Ok(DrainEnd::Deadline),
        }
    }
}

async fn drain_to_action(
    deadline: Instant,
    rx: &mut mpsc::UnboundedReceiver<IpcEvent>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    match drain_until(deadline, rx, telemetry, out).await? {
        DrainEnd::Deadline => Ok(()),
        DrainEnd::ObserverClosed => {
            Err("mpv IPC observer closed before a scheduled action".to_string())
        }
    }
}

async fn wait_for_instance(wait: Duration) -> Result<yututui::remote::proto::InstanceFile, String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(instance) = yututui::remote::endpoint::read_instance() {
            return Ok(instance);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the isolated ytt remote descriptor".to_string());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn discover_mpv(app_pid: u32, wait: Duration) -> Result<(u32, String), String> {
    let deadline = Instant::now() + wait;
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let mut descendants = std::collections::HashSet::from([app_pid]);
        loop {
            let before = descendants.len();
            for (pid, process) in system.processes() {
                if process
                    .parent()
                    .is_some_and(|parent| descendants.contains(&parent.as_u32()))
                {
                    descendants.insert(pid.as_u32());
                }
            }
            if descendants.len() == before {
                break;
            }
        }
        for pid in descendants {
            if pid == app_pid {
                continue;
            }
            let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
                continue;
            };
            let is_mpv = process
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("mpv");
            if !is_mpv {
                continue;
            }
            let endpoint = process.cmd().iter().find_map(|arg| {
                arg.to_string_lossy()
                    .strip_prefix("--input-ipc-server=")
                    .map(str::to_owned)
            });
            if let Some(endpoint) = endpoint {
                return Ok((pid, endpoint));
            }
            return Err(format!(
                "mpv PID {pid} was found but its command line did not expose --input-ipc-server"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for an mpv descendant of ytt PID {app_pid}"
            ));
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn connect_mpv(endpoint: &str, wait: Duration) -> Result<Stream, String> {
    let deadline = Instant::now() + wait;
    loop {
        let name = endpoint
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("invalid mpv IPC endpoint: {e}"))?;
        match Stream::connect(name).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(format!("connect to mpv IPC: {error}")),
        }
    }
}

async fn subscribe(stream: &Stream) -> Result<(), String> {
    for (id, property) in [(9_001u64, "paused-for-cache"), (9_002, "time-pos")] {
        let mut writer = stream;
        let mut payload = serde_json::to_vec(&json!({
            "command": ["observe_property", id, property],
            "request_id": id,
        }))
        .map_err(|e| format!("encode mpv subscription: {e}"))?;
        payload.push(b'\n');
        writer
            .write_all(&payload)
            .await
            .map_err(|e| format!("write mpv subscription: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("flush mpv subscription: {e}"))?;
    }
    Ok(())
}

async fn read_mpv(stream: Stream, tx: mpsc::UnboundedSender<IpcEvent>) {
    let mut reader = BufReader::new(&stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match yututui::util::io::read_bounded_line(&mut reader, &mut line, IPC_LINE_CAP).await {
            Ok(yututui::util::io::BoundedLine::Line) => {
                if let Ok(value) = serde_json::from_slice::<Value>(&line)
                    && tx
                        .send(IpcEvent {
                            observed_at: Instant::now(),
                            value,
                        })
                        .is_err()
                {
                    return;
                }
            }
            Ok(yututui::util::io::BoundedLine::Eof)
            | Ok(yututui::util::io::BoundedLine::TooLarge)
            | Err(_) => return,
        }
    }
}

async fn perform_load(
    command: RemoteCommand,
    name: &str,
    wait: Duration,
    rx: &mut mpsc::UnboundedReceiver<IpcEvent>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    let started = Instant::now();
    send_remote(command).await?;
    wait_for_mpv(wait, rx, telemetry, out, |value| {
        value.get("event").and_then(Value::as_str) == Some("playback-restart")
    })
    .await?;
    operation(out, name, started.elapsed(), json!({}))
}

async fn wait_for_mpv(
    wait: Duration,
    rx: &mut mpsc::UnboundedReceiver<IpcEvent>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
    predicate: impl Fn(&Value) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = timeout(remaining, rx.recv())
            .await
            .map_err(|_| "timed out waiting for mpv playback telemetry".to_string())?
            .ok_or_else(|| "mpv IPC observer closed unexpectedly".to_string())?;
        let matched = predicate(&event.value);
        telemetry.record(&event, out)?;
        if matched {
            return Ok(());
        }
    }
}

async fn send_remote(command: RemoteCommand) -> Result<(), String> {
    let response = yututui::remote::client::send(command)
        .await
        .map_err(|e| format!("remote transport: {}", e.human_message()))?;
    if response.ok {
        Ok(())
    } else {
        Err(format!(
            "remote command rejected: {}",
            response.reason.as_deref().unwrap_or("rejected")
        ))
    }
}

async fn status() -> Result<StatusSnapshot, String> {
    let response = yututui::remote::client::send(RemoteCommand::Status)
        .await
        .map_err(|e| format!("status transport: {}", e.human_message()))?;
    response
        .status
        .ok_or_else(|| "status response did not contain a snapshot".to_string())
}

async fn wait_for_status(
    wait: Duration,
    predicate: impl Fn(&StatusSnapshot) -> bool,
) -> Result<StatusSnapshot, String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(snapshot) = status().await
            && predicate(&snapshot)
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for remote playback state".to_string());
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn ensure_playing(wait: Duration) -> Result<(), String> {
    let snapshot = status().await?;
    if snapshot.paused {
        send_remote(RemoteCommand::TogglePause).await?;
        wait_for_status(wait, |state| !state.paused).await?;
    }
    Ok(())
}

fn operation(
    out: &mut BufWriter<File>,
    operation: &str,
    latency: Duration,
    detail: Value,
) -> Result<(), String> {
    write_ndjson(
        out,
        &json!({
            "schema": SCHEMA,
            "kind": "operation",
            "operation": operation,
            "latency_ms": latency.as_secs_f64() * 1_000.0,
            "detail": detail,
        }),
    )
}

fn write_ndjson(out: &mut BufWriter<File>, value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value).map_err(|e| format!("encode NDJSON: {e}"))?;
    out.write_all(b"\n")
        .map_err(|e| format!("write NDJSON: {e}"))?;
    out.flush().map_err(|e| format!("flush NDJSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_actions_are_spread_across_the_observation_window() {
        let window = Duration::from_secs(70);
        assert_eq!(scheduled_offset(window, 1, 6), Duration::from_secs(10));
        assert_eq!(scheduled_offset(window, 3, 6), Duration::from_secs(30));
        assert_eq!(scheduled_offset(window, 6, 6), Duration::from_secs(60));
    }

    #[test]
    fn explicit_no_pause_keeps_steady_soak_free_of_pause_actions() {
        assert_eq!(scheduled_action_count(0, None), 0);
        assert_eq!(
            scheduled_action_count(5, Some(Duration::from_millis(500))),
            6
        );
    }

    #[test]
    fn resume_progress_rejects_pause_boundary_jitter() {
        let paused = 66.429_486;
        let time_pos = |position| {
            json!({
                "event": "property-change",
                "name": "time-pos",
                "data": position,
            })
        };

        assert!(!proves_resume_progress(&time_pos(66.429_563), paused));
        assert!(proves_resume_progress(&time_pos(66.526_510), paused));
        assert!(!proves_resume_progress(
            &json!({"event": "playback-restart"}),
            paused
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_until_keeps_collecting_buffering_for_the_full_window() {
        let path = std::env::temp_dir().join(format!(
            "ytt-tui-perf-control-{}-{}.ndjson",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock must be after epoch")
                .as_nanos()
        ));
        let mut out = BufWriter::new(File::create(&path).expect("create test output"));
        let origin = Instant::now();
        let mut telemetry = Telemetry::new(origin);
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(IpcEvent {
            observed_at: origin + Duration::from_millis(2),
            value: json!({
                "event": "property-change",
                "name": "paused-for-cache",
                "data": true,
            }),
        })
        .expect("send buffering start");
        tx.send(IpcEvent {
            observed_at: origin + Duration::from_millis(7),
            value: json!({
                "event": "property-change",
                "name": "paused-for-cache",
                "data": false,
            }),
        })
        .expect("send buffering end");

        let drain_started = Instant::now();
        let end = drain_until(
            drain_started + Duration::from_millis(40),
            &mut rx,
            &mut telemetry,
            &mut out,
        )
        .await
        .expect("drain observation window");

        assert_eq!(end, DrainEnd::Deadline);
        assert!(drain_started.elapsed() >= Duration::from_millis(30));
        assert_eq!(telemetry.buffering_events, 1);
        assert_eq!(telemetry.buffering_total, Duration::from_millis(5));
        drop(tx);
        drop(out);
        std::fs::remove_file(path).expect("remove test output");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clean_mpv_close_finishes_after_queued_telemetry_is_recorded() {
        let path = std::env::temp_dir().join(format!(
            "ytt-tui-perf-control-close-{}-{}.ndjson",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock must be after epoch")
                .as_nanos()
        ));
        let mut out = BufWriter::new(File::create(&path).expect("create test output"));
        let origin = Instant::now();
        let mut telemetry = Telemetry::new(origin);
        let (tx, mut rx) = mpsc::unbounded_channel();
        for (elapsed_ms, paused) in [(2, true), (9, false)] {
            tx.send(IpcEvent {
                observed_at: origin + Duration::from_millis(elapsed_ms),
                value: json!({
                    "event": "property-change",
                    "name": "paused-for-cache",
                    "data": paused,
                }),
            })
            .expect("queue telemetry before close");
        }
        drop(tx);

        let end = drain_until(
            Instant::now() + Duration::from_secs(1),
            &mut rx,
            &mut telemetry,
            &mut out,
        )
        .await
        .expect("clean observer close is a valid final boundary");

        assert_eq!(end, DrainEnd::ObserverClosed);
        assert_eq!(telemetry.buffering_events, 1);
        assert_eq!(telemetry.buffering_total, Duration::from_millis(7));
        drop(out);
        std::fs::remove_file(path).expect("remove test output");
    }
}
