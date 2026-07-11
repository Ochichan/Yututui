//! Out-of-band playback controller for the TUI performance harness.
//!
//! User actions go through the existing authenticated remote protocol. A second,
//! read-only mpv IPC connection observes `playback-restart`, `paused-for-cache`, and
//! `time-pos` so latency and buffering are measured without injecting terminal keys.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::io::{AsyncRead, AsyncWriteExt, BufReader};
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
    ready_file: PathBuf,
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
        let mut ready_file = None;
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
                "--ready-file" => ready_file = Some(PathBuf::from(next("--ready-file", &mut raw)?)),
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
            ready_file: ready_file.ok_or_else(|| "--ready-file is required".to_string())?,
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
    "Usage: tui_perf_control --output FILE --ready-file FILE [options]\n\
     Options:\n\
       --wait-secs N                 Per-operation timeout (default 30)\n\
       --ready-file FILE             Atomic sampler subscription barrier\n\
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

#[derive(Debug)]
enum IpcMessage {
    Event(IpcEvent),
    CleanEof {
        observed_at: Instant,
    },
    TooLarge {
        observed_at: Instant,
    },
    ParseError {
        observed_at: Instant,
        message: String,
    },
    IoError {
        observed_at: Instant,
        message: String,
    },
}

#[derive(Clone, Debug)]
struct ProcessIdentity {
    pid: u32,
    start_time_unix_s: u64,
    executable: PathBuf,
    executable_bytes: u64,
    executable_sha256: String,
}

#[derive(Clone, Debug)]
struct MpvBinding {
    process: ProcessIdentity,
    endpoint: String,
}

struct Telemetry {
    origin: Instant,
    buffering_cutoff: Instant,
    buffering_since: Option<Instant>,
    buffering_events: u64,
    buffering_total: Duration,
    last_time_pos_s: Option<f64>,
    cutoff_first_time_pos: Option<(Instant, f64)>,
    cutoff_last_time_pos: Option<(Instant, f64)>,
    first_event_ns: Option<u128>,
    last_event_ns: Option<u128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainEnd {
    Deadline,
    CleanEof(Instant),
}

impl DrainEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::CleanEof(_) => "mpv_ipc_closed",
        }
    }
}

impl Telemetry {
    fn new(origin: Instant, buffering_cutoff: Instant) -> Self {
        Self {
            origin,
            buffering_cutoff,
            buffering_since: None,
            buffering_events: 0,
            buffering_total: Duration::ZERO,
            last_time_pos_s: None,
            cutoff_first_time_pos: None,
            cutoff_last_time_pos: None,
            first_event_ns: None,
            last_event_ns: None,
        }
    }

    fn record(&mut self, event: &IpcEvent, out: &mut BufWriter<File>) -> Result<(), String> {
        let elapsed_ns = event
            .observed_at
            .saturating_duration_since(self.origin)
            .as_nanos();
        self.first_event_ns.get_or_insert(elapsed_ns);
        self.last_event_ns = Some(elapsed_ns);
        if let Some(position) = time_pos_seconds(&event.value) {
            self.record_time_pos(event.observed_at, position);
        }
        self.record_buffering(event.observed_at, &event.value);
        write_ndjson(
            out,
            &json!({
                "schema": SCHEMA,
                "kind": "mpv_event",
                "elapsed_ms": event.observed_at.saturating_duration_since(self.origin).as_millis(),
                "elapsed_ns": elapsed_ns,
                "event": event.value,
            }),
        )
    }

    fn record_buffering(&mut self, observed_at: Instant, value: &Value) {
        if observed_at >= self.buffering_cutoff {
            self.finish_buffering(self.buffering_cutoff);
        } else if value.get("event").and_then(Value::as_str) == Some("property-change")
            && value.get("name").and_then(Value::as_str) == Some("paused-for-cache")
            && let Some(paused) = value.get("data").and_then(Value::as_bool)
        {
            if paused && self.buffering_since.is_none() {
                self.buffering_since = Some(observed_at);
                self.buffering_events += 1;
            } else if !paused && let Some(started) = self.buffering_since.take() {
                self.buffering_total += observed_at.saturating_duration_since(started);
            }
        }
    }

    fn record_time_pos(&mut self, observed_at: Instant, position: f64) {
        self.last_time_pos_s = Some(position);
        if observed_at <= self.buffering_cutoff {
            self.cutoff_first_time_pos
                .get_or_insert((observed_at, position));
            self.cutoff_last_time_pos = Some((observed_at, position));
        }
    }

    fn finish(&mut self, now: Instant) {
        self.finish_buffering(now.min(self.buffering_cutoff));
    }

    fn finish_buffering(&mut self, end: Instant) {
        if let Some(started) = self.buffering_since.take() {
            self.buffering_total += end.saturating_duration_since(started);
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
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    while let Ok(message) = rx.try_recv() {
        match message {
            IpcMessage::Event(event) => telemetry.record(&event, out)?,
            terminal => return Err(ipc_terminal_error(terminal)),
        }
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
    let run_id =
        std::env::var("TUI_PERF_RUN_ID").map_err(|_| "TUI_PERF_RUN_ID is required".to_string())?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }
    let mut out = BufWriter::new(
        File::create(&args.output).map_err(|e| format!("create {}: {e}", args.output.display()))?,
    );
    let producer_binary_hash = producer_binary_sha256()?;
    let instance = wait_for_instance(args.wait).await?;
    let (owner, mpv) = discover_mpv(instance.app_pid, args.wait).await?;
    let stream = connect_mpv(&mpv.endpoint, args.wait).await?;
    let reader = subscribe_and_confirm(stream, args.wait).await?;
    let observation_started_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before Unix epoch: {e}"))?
        .as_nanos();
    let origin = Instant::now();
    publish_ready(
        &args.ready_file,
        &owner,
        &mpv,
        &run_id,
        observation_started_unix_ns,
    )?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_mpv(reader, tx));
    let mut telemetry = Telemetry::new(origin, origin + args.observe);

    write_ndjson(
        &mut out,
        &header_record(
            &args,
            &owner,
            instance.mode,
            &mpv,
            &producer_binary_hash,
            &run_id,
            observation_started_unix_ns,
        ),
    )?;

    match &args.load {
        LoadAction::None => {
            let started = Instant::now();
            let status = wait_for_status(args.wait, |status| status.title.is_some()).await?;
            let completed = Instant::now();
            operation(
                &mut out,
                origin,
                "ready",
                started..completed,
                "status",
                None,
                json!({"title": status.title, "status_field": "title", "status_present": true}),
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
                RemoteCommand::Play {
                    query: query.clone(),
                },
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
    for (index, target_s) in args.seeks.iter().copied().enumerate() {
        if scheduled_actions > 0 && !args.observe.is_zero() {
            let offset = scheduled_offset(args.observe, index + 1, scheduled_actions);
            drain_to_action(origin + offset, &mut rx, &mut telemetry, &mut out).await?;
        }
        drain_pending_mpv(&mut rx, &mut telemetry, &mut out)?;
        let started = Instant::now();
        send_remote(RemoteCommand::SeekTo {
            ms: (target_s * 1_000.0).round() as u64,
        })
        .await?;
        let (restart, completed, observed_target_s) = wait_for_seek(
            args.wait,
            &mut rx,
            &mut telemetry,
            &mut out,
            started,
            target_s,
        )
        .await?;
        operation(
            &mut out,
            origin,
            "seek",
            started..completed.observed_at,
            "mpv_event",
            Some(&completed),
            json!({
                "target_s": target_s,
                "observed_target_s": observed_target_s,
                "playback_restart_elapsed_ns": restart
                    .observed_at
                    .saturating_duration_since(origin)
                    .as_nanos(),
                "target_tolerance_s": 2.0,
            }),
        )?;
    }

    if let Some(hold) = args.pause_hold {
        if scheduled_actions > 0 && !args.observe.is_zero() {
            let offset = scheduled_offset(args.observe, scheduled_actions, scheduled_actions);
            drain_to_action(origin + offset, &mut rx, &mut telemetry, &mut out).await?;
        }
        ensure_playing(args.wait).await?;
        drain_pending_mpv(&mut rx, &mut telemetry, &mut out)?;
        let pause_started = Instant::now();
        send_remote(RemoteCommand::TogglePause).await?;
        wait_for_status(args.wait, |status| status.paused).await?;
        let pause_completed = Instant::now();
        operation(
            &mut out,
            origin,
            "pause",
            pause_started..pause_completed,
            "status",
            None,
            json!({"status_field": "paused", "status_value": true}),
        )?;
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
        let resume_completed = wait_for_mpv(
            args.wait,
            &mut rx,
            &mut telemetry,
            &mut out,
            resume_started,
            |value| proves_resume_progress(value, paused_time_pos_s),
        )
        .await?;
        operation(
            &mut out,
            origin,
            "resume",
            resume_started..resume_completed.observed_at,
            "mpv_event",
            Some(&resume_completed),
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
        origin + args.observe,
        &mut rx,
        &mut telemetry,
        &mut out,
    )
    .await?;

    while let Ok(message) = rx.try_recv() {
        match message {
            IpcMessage::Event(event) => telemetry.record(&event, &mut out)?,
            IpcMessage::CleanEof { .. } if matches!(observation_end, DrainEnd::CleanEof(_)) => {}
            terminal => return Err(ipc_terminal_error(terminal)),
        }
    }
    let summary_at = Instant::now();
    telemetry.finish(summary_at);
    write_ndjson(
        &mut out,
        &summary_record(
            &telemetry,
            &args,
            origin,
            summary_at,
            observation_end,
            &run_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("system clock before Unix epoch: {e}"))?
                .as_nanos(),
        ),
    )?;
    out.flush().map_err(|e| format!("flush output: {e}"))
}

async fn drain_until(
    deadline: Instant,
    minimum_clean_eof: Instant,
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<DrainEnd, String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(DrainEnd::Deadline);
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(IpcMessage::Event(event))) => telemetry.record(&event, out)?,
            Ok(Some(IpcMessage::CleanEof { observed_at })) => {
                if observed_at < minimum_clean_eof {
                    return Err("mpv IPC closed before the declared observation window".to_string());
                }
                return Ok(DrainEnd::CleanEof(observed_at));
            }
            Ok(Some(terminal)) => return Err(ipc_terminal_error(terminal)),
            Ok(None) => {
                return Err(
                    "mpv IPC observer task disappeared without a terminal record".to_string(),
                );
            }
            Err(_) => return Ok(DrainEnd::Deadline),
        }
    }
}

async fn drain_to_action(
    deadline: Instant,
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    match drain_until(deadline, deadline, rx, telemetry, out).await? {
        DrainEnd::Deadline => Ok(()),
        DrainEnd::CleanEof(_) => {
            Err("mpv IPC observer closed before a scheduled action".to_string())
        }
    }
}

fn ipc_terminal_error(message: IpcMessage) -> String {
    match message {
        IpcMessage::Event(_) => "internal IPC event classification error".to_string(),
        IpcMessage::CleanEof { .. } => "mpv IPC closed unexpectedly".to_string(),
        IpcMessage::TooLarge { observed_at } => {
            format!("mpv IPC emitted an oversized line at {observed_at:?}")
        }
        IpcMessage::ParseError {
            observed_at,
            message,
        } => {
            format!("mpv IPC JSON parse error at {observed_at:?}: {message}")
        }
        IpcMessage::IoError {
            observed_at,
            message,
        } => {
            format!("mpv IPC I/O error at {observed_at:?}: {message}")
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

async fn discover_mpv(
    app_pid: u32,
    wait: Duration,
) -> Result<(ProcessIdentity, MpvBinding), String> {
    let deadline = Instant::now() + wait;
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let Some(owner_process) = system.process(sysinfo::Pid::from_u32(app_pid)) else {
            if Instant::now() >= deadline {
                return Err(format!("timed out resolving ytt owner PID {app_pid}"));
            }
            sleep(Duration::from_millis(25)).await;
            continue;
        };
        let owner = process_identity(app_pid, owner_process)?;
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
                return Ok((
                    owner,
                    MpvBinding {
                        process: process_identity(pid, process)?,
                        endpoint,
                    },
                ));
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

fn process_identity(pid: u32, process: &sysinfo::Process) -> Result<ProcessIdentity, String> {
    let executable = process
        .exe()
        .ok_or_else(|| format!("PID {pid} has no executable path"))?
        .canonicalize()
        .map_err(|e| format!("canonicalize executable for PID {pid}: {e}"))?;
    let executable_bytes = executable
        .metadata()
        .map_err(|e| {
            format!(
                "stat executable for PID {pid} {}: {e}",
                executable.display()
            )
        })?
        .len();
    let executable_sha256 = sha256_file(&executable)?;
    Ok(ProcessIdentity {
        pid,
        start_time_unix_s: process.start_time(),
        executable,
        executable_bytes,
        executable_sha256,
    })
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

async fn subscribe_and_confirm(
    mut stream: Stream,
    wait: Duration,
) -> Result<BufReader<Stream>, String> {
    for (id, property) in [(9_001u64, "paused-for-cache"), (9_002, "time-pos")] {
        let mut payload = serde_json::to_vec(&json!({
            "command": ["observe_property", id, property],
            "request_id": id,
        }))
        .map_err(|e| format!("encode mpv subscription: {e}"))?;
        payload.push(b'\n');
        stream
            .write_all(&payload)
            .await
            .map_err(|e| format!("write mpv subscription: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("flush mpv subscription: {e}"))?;
    }
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + wait;
    let mut confirmed = std::collections::HashSet::new();
    let mut line = Vec::new();
    while confirmed.len() < 2 {
        line.clear();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out confirming mpv property subscriptions".to_string());
        }
        let bounded = timeout(
            remaining,
            yututui::util::io::read_bounded_line(&mut reader, &mut line, IPC_LINE_CAP),
        )
        .await
        .map_err(|_| "timed out confirming mpv property subscriptions".to_string())?
        .map_err(|e| format!("read mpv subscription response: {e}"))?;
        match bounded {
            yututui::util::io::BoundedLine::Line => {
                let value: Value = serde_json::from_slice(&line)
                    .map_err(|e| format!("parse mpv subscription response: {e}"))?;
                let Some(request_id) = value.get("request_id").and_then(Value::as_u64) else {
                    continue;
                };
                if !matches!(request_id, 9_001 | 9_002) {
                    continue;
                }
                if value.get("error").and_then(Value::as_str) != Some("success") {
                    return Err(format!(
                        "mpv subscription {request_id} failed: {}",
                        value.get("error").unwrap_or(&Value::Null)
                    ));
                }
                confirmed.insert(request_id);
            }
            yututui::util::io::BoundedLine::Eof => {
                return Err("mpv IPC closed before subscription confirmation".to_string());
            }
            yututui::util::io::BoundedLine::TooLarge => {
                return Err("oversized mpv subscription response".to_string());
            }
        }
    }
    Ok(reader)
}

async fn read_mpv<R>(mut reader: BufReader<R>, tx: mpsc::UnboundedSender<IpcMessage>)
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        match yututui::util::io::read_bounded_line(&mut reader, &mut line, IPC_LINE_CAP).await {
            Ok(yututui::util::io::BoundedLine::Line) => {
                match serde_json::from_slice::<Value>(&line) {
                    Ok(value) => {
                        if tx
                            .send(IpcMessage::Event(IpcEvent {
                                observed_at: Instant::now(),
                                value,
                            }))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(IpcMessage::ParseError {
                            observed_at: Instant::now(),
                            message: error.to_string(),
                        });
                        return;
                    }
                }
            }
            Ok(yututui::util::io::BoundedLine::Eof) => {
                let observed_at = Instant::now();
                if line.is_empty() {
                    let _ = tx.send(IpcMessage::CleanEof { observed_at });
                } else {
                    let _ = tx.send(IpcMessage::ParseError {
                        observed_at,
                        message: format!(
                            "mpv IPC closed with a truncated JSON frame ({} bytes)",
                            line.len()
                        ),
                    });
                }
                return;
            }
            Ok(yututui::util::io::BoundedLine::TooLarge) => {
                let _ = tx.send(IpcMessage::TooLarge {
                    observed_at: Instant::now(),
                });
                return;
            }
            Err(error) => {
                let _ = tx.send(IpcMessage::IoError {
                    observed_at: Instant::now(),
                    message: error.to_string(),
                });
                return;
            }
        }
    }
}

async fn perform_load(
    command: RemoteCommand,
    name: &str,
    wait: Duration,
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    drain_pending_mpv(rx, telemetry, out)?;
    let started = Instant::now();
    send_remote(command).await?;
    let completed = wait_for_mpv(wait, rx, telemetry, out, started, |value| {
        value.get("event").and_then(Value::as_str) == Some("playback-restart")
    })
    .await?;
    operation(
        out,
        telemetry.origin,
        name,
        started..completed.observed_at,
        "mpv_event",
        Some(&completed),
        json!({}),
    )
}

async fn wait_for_mpv(
    wait: Duration,
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
    operation_started: Instant,
    predicate: impl Fn(&Value) -> bool,
) -> Result<IpcEvent, String> {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = timeout(remaining, rx.recv())
            .await
            .map_err(|_| "timed out waiting for mpv playback telemetry".to_string())?
            .ok_or_else(|| "mpv IPC observer closed unexpectedly".to_string())?;
        let event = match message {
            IpcMessage::Event(event) => event,
            terminal => return Err(ipc_terminal_error(terminal)),
        };
        let matched = predicate(&event.value);
        telemetry.record(&event, out)?;
        if matched && event.observed_at >= operation_started {
            return Ok(event);
        }
    }
}

async fn wait_for_seek(
    wait: Duration,
    rx: &mut mpsc::UnboundedReceiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
    operation_started: Instant,
    target_s: f64,
) -> Result<(IpcEvent, IpcEvent, f64), String> {
    let deadline = Instant::now() + wait;
    let mut restart = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = timeout(remaining, rx.recv())
            .await
            .map_err(|_| "timed out waiting for seek completion telemetry".to_string())?
            .ok_or_else(|| "mpv IPC observer closed unexpectedly".to_string())?;
        let event = match message {
            IpcMessage::Event(event) => event,
            terminal => return Err(ipc_terminal_error(terminal)),
        };
        let is_post_command = event.observed_at >= operation_started;
        let is_restart =
            event.value.get("event").and_then(Value::as_str) == Some("playback-restart");
        let position = time_pos_seconds(&event.value);
        telemetry.record(&event, out)?;
        if is_post_command && is_restart && restart.is_none() {
            restart = Some(event);
            continue;
        }
        if is_post_command
            && restart.is_some()
            && let Some(position) = position
            && (position - target_s).abs() <= 2.0
        {
            return Ok((restart.expect("checked above"), event, position));
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
    origin: Instant,
    operation: &str,
    interval: std::ops::Range<Instant>,
    completion_source: &str,
    completion_event: Option<&IpcEvent>,
    detail: Value,
) -> Result<(), String> {
    let started = interval.start;
    let completed = interval.end;
    let started_ns = started.saturating_duration_since(origin).as_nanos();
    let completed_ns = completed.saturating_duration_since(origin).as_nanos();
    let completion_event_elapsed_ns = completion_event.map(|event| {
        event
            .observed_at
            .saturating_duration_since(origin)
            .as_nanos()
    });
    let completion_event_type = completion_event.and_then(|event| {
        event
            .value
            .get("event")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let completion_property = completion_event.and_then(|event| {
        event
            .value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    write_ndjson(
        out,
        &json!({
            "schema": SCHEMA,
            "kind": "operation",
            "operation": operation,
            "operation_started_ns": started_ns,
            "operation_completed_ns": completed_ns,
            "latency_ms": (completed_ns - started_ns) as f64 / 1_000_000.0,
            "completion_source": completion_source,
            "completion_event_elapsed_ns": completion_event_elapsed_ns,
            "completion_event_type": completion_event_type,
            "completion_property": completion_property,
            "detail": detail,
        }),
    )
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn producer_binary_sha256() -> Result<String, String> {
    let executable = std::env::current_exe()
        .map_err(|e| format!("resolve current controller executable: {e}"))?;
    sha256_file(&executable)
}

fn header_record(
    args: &Args,
    owner: &ProcessIdentity,
    owner_mode: yututui::remote::proto::InstanceMode,
    mpv: &MpvBinding,
    producer_binary_hash: &str,
    run_id: &str,
    observation_started_unix_ns: u128,
) -> Value {
    json!({
        "schema": SCHEMA,
        "kind": "header",
        "owner_pid": owner.pid,
        "owner_start_time_unix_s": owner.start_time_unix_s,
        "owner_executable": owner.executable,
        "owner_executable_bytes": owner.executable_bytes,
        "owner_executable_sha256": owner.executable_sha256,
        "owner_mode": owner_mode,
        "mpv_pid": mpv.process.pid,
        "mpv_start_time_unix_s": mpv.process.start_time_unix_s,
        "mpv_executable": mpv.process.executable,
        "mpv_executable_bytes": mpv.process.executable_bytes,
        "mpv_executable_sha256": mpv.process.executable_sha256,
        "mpv_endpoint": mpv.endpoint,
        "producer_binary_sha256": producer_binary_hash,
        "run_id": run_id,
        "observation_started_unix_ns": observation_started_unix_ns,
        "observe_ns": args.observe.as_nanos(),
        "buffering_cutoff_ns": args.observe.as_nanos(),
        "close_grace_ns": args.close_grace.as_nanos(),
        "subscriptions_confirmed": true,
        "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pause_policy": if args.pause_hold.is_some() { "pause-resume" } else { "none" },
        "pause_hold_ms": args.pause_hold.map(|hold| hold.as_millis()),
    })
}

fn summary_record(
    telemetry: &Telemetry,
    args: &Args,
    origin: Instant,
    summary_at: Instant,
    observation_end: DrainEnd,
    run_id: &str,
    observation_finished_unix_ns: u128,
) -> Value {
    let elapsed_ns = summary_at.saturating_duration_since(origin).as_nanos();
    let terminal_observed_ns = match observation_end {
        DrainEnd::Deadline => None,
        DrainEnd::CleanEof(observed_at) => {
            Some(observed_at.saturating_duration_since(origin).as_nanos())
        }
    };
    let (cutoff_first_time_pos_ns, cutoff_first_time_pos_s) = telemetry
        .cutoff_first_time_pos
        .map(|(observed_at, position)| {
            (
                observed_at.saturating_duration_since(origin).as_nanos(),
                position,
            )
        })
        .unzip();
    let (cutoff_last_time_pos_ns, cutoff_last_time_pos_s) = telemetry
        .cutoff_last_time_pos
        .map(|(observed_at, position)| {
            (
                observed_at.saturating_duration_since(origin).as_nanos(),
                position,
            )
        })
        .unzip();
    json!({
        "schema": SCHEMA,
        "kind": "summary",
        "run_id": run_id,
        "observation_finished_unix_ns": observation_finished_unix_ns,
        "elapsed_ns": elapsed_ns,
        "expected_observation_ns": args.observe.as_nanos(),
        "buffering_cutoff_ns": args.observe.as_nanos(),
        "actual_coverage_ns": elapsed_ns.min(args.observe.as_nanos()),
        "first_event_ns": telemetry.first_event_ns,
        "last_event_ns": telemetry.last_event_ns,
        "terminal_observed_ns": terminal_observed_ns,
        "terminal_kind": match observation_end {
            DrainEnd::Deadline => "deadline",
            DrainEnd::CleanEof(_) => "clean_eof",
        },
        "buffering_events": telemetry.buffering_events,
        "buffering_ms": telemetry.buffering_total.as_millis(),
        "cutoff_first_time_pos_ns": cutoff_first_time_pos_ns,
        "cutoff_first_time_pos_s": cutoff_first_time_pos_s,
        "cutoff_last_time_pos_ns": cutoff_last_time_pos_ns,
        "cutoff_last_time_pos_s": cutoff_last_time_pos_s,
        "observation_end": observation_end.as_str(),
    })
}

fn publish_ready(
    path: &Path,
    owner: &ProcessIdentity,
    mpv: &MpvBinding,
    run_id: &str,
    observation_started_unix_ns: u128,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create ready directory {}: {e}", parent.display()))?;
    }
    if path.exists() {
        return Err(format!(
            "controller ready path already exists: {}",
            path.display()
        ));
    }
    let temporary = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(&json!({
        "schema": "ytt.tui-perf.controller-ready.v1",
        "run_id": run_id,
        "owner_pid": owner.pid,
        "owner_start_time_unix_s": owner.start_time_unix_s,
        "mpv_pid": mpv.process.pid,
        "mpv_start_time_unix_s": mpv.process.start_time_unix_s,
        "mpv_endpoint": mpv.endpoint,
        "subscriptions_confirmed": true,
        "observation_started_unix_ns": observation_started_unix_ns,
        "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
    }))
    .map_err(|e| format!("encode controller ready file: {e}"))?;
    std::fs::write(&temporary, bytes)
        .map_err(|e| format!("write controller ready file {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|e| format!("publish controller ready file {}: {e}", path.display()))
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
    fn header_records_the_current_controller_binary_sha256() {
        let executable = std::env::current_exe().expect("resolve current test executable");
        let expected = sha256_file(&executable).expect("hash current test executable");
        let producer_hash = producer_binary_sha256().expect("hash current controller executable");
        let args = Args {
            output: PathBuf::from("control.ndjson"),
            ready_file: PathBuf::from("controller-ready.json"),
            wait: Duration::from_secs(1),
            observe: Duration::from_secs(2),
            close_grace: Duration::ZERO,
            load: LoadAction::None,
            seeks: Vec::new(),
            pause_hold: None,
        };
        let identity = ProcessIdentity {
            pid: 42,
            start_time_unix_s: 1,
            executable: executable.clone(),
            executable_bytes: executable.metadata().expect("stat test executable").len(),
            executable_sha256: expected.clone(),
        };
        let mpv = MpvBinding {
            process: ProcessIdentity {
                pid: 43,
                ..identity.clone()
            },
            endpoint: "mpv.sock".to_string(),
        };
        let header = header_record(
            &args,
            &identity,
            yututui::remote::proto::InstanceMode::StandaloneTui,
            &mpv,
            &producer_hash,
            "self-test-run",
            123,
        );

        assert_eq!(producer_hash, expected);
        assert_eq!(producer_hash.len(), 64);
        assert_eq!(header["kind"], "header");
        assert_eq!(header["producer_binary_sha256"], producer_hash);
        assert_eq!(header["run_id"], "self-test-run");
    }

    #[test]
    fn summary_records_the_monotonic_finish_boundary() {
        let origin = Instant::now();
        let summary_at = origin + Duration::from_millis(7);
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(5));
        telemetry.buffering_since = Some(origin + Duration::from_millis(2));
        telemetry.buffering_events = 1;
        telemetry.finish(summary_at);

        let args = Args {
            output: PathBuf::from("control.ndjson"),
            ready_file: PathBuf::from("controller-ready.json"),
            wait: Duration::from_secs(1),
            observe: Duration::from_millis(5),
            close_grace: Duration::from_millis(5),
            load: LoadAction::None,
            seeks: Vec::new(),
            pause_hold: None,
        };
        let summary = summary_record(
            &telemetry,
            &args,
            origin,
            summary_at,
            DrainEnd::CleanEof(summary_at),
            "self-test-run",
            456,
        );

        assert_eq!(summary["elapsed_ns"], 7_000_000u64);
        assert_eq!(summary["buffering_events"], 1);
        assert_eq!(summary["buffering_ms"], 3);
        assert_eq!(summary["buffering_cutoff_ns"], 5_000_000u64);
        assert_eq!(summary["observation_end"], "mpv_ipc_closed");
        assert_eq!(summary["run_id"], "self-test-run");
        assert_eq!(summary["observation_finished_unix_ns"], 456);
    }

    #[test]
    fn scheduled_actions_are_spread_across_the_observation_window() {
        let window = Duration::from_secs(70);
        assert_eq!(scheduled_offset(window, 1, 6), Duration::from_secs(10));
        assert_eq!(scheduled_offset(window, 3, 6), Duration::from_secs(30));
        assert_eq!(scheduled_offset(window, 6, 6), Duration::from_secs(60));
    }

    #[test]
    fn buffering_after_the_metric_cutoff_is_ignored() {
        let origin = Instant::now();
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(5));
        for (elapsed_ms, paused) in [(6, true), (7, false)] {
            telemetry.record_buffering(
                origin + Duration::from_millis(elapsed_ms),
                &json!({
                    "event": "property-change",
                    "name": "paused-for-cache",
                    "data": paused,
                }),
            );
        }

        assert_eq!(telemetry.buffering_events, 0);
        assert_eq!(telemetry.buffering_total, Duration::ZERO);
    }

    #[test]
    fn buffering_interval_crossing_the_metric_cutoff_is_clipped() {
        let origin = Instant::now();
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(5));
        for (elapsed_ms, paused) in [(2, true), (7, false)] {
            telemetry.record_buffering(
                origin + Duration::from_millis(elapsed_ms),
                &json!({
                    "event": "property-change",
                    "name": "paused-for-cache",
                    "data": paused,
                }),
            );
        }

        assert_eq!(telemetry.buffering_events, 1);
        assert_eq!(telemetry.buffering_total, Duration::from_millis(3));
    }

    #[test]
    fn time_pos_summary_tail_is_bound_to_the_metric_cutoff() {
        let origin = Instant::now();
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(5));
        telemetry.record_time_pos(origin + Duration::from_millis(1), 10.0);
        telemetry.record_time_pos(origin + Duration::from_millis(4), 13.0);
        telemetry.record_time_pos(origin + Duration::from_millis(7), 99.0);

        assert_eq!(telemetry.last_time_pos_s, Some(99.0));
        assert_eq!(
            telemetry.cutoff_first_time_pos,
            Some((origin + Duration::from_millis(1), 10.0))
        );
        assert_eq!(
            telemetry.cutoff_last_time_pos,
            Some((origin + Duration::from_millis(4), 13.0))
        );
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
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(40));
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(IpcMessage::Event(IpcEvent {
            observed_at: origin + Duration::from_millis(2),
            value: json!({
                "event": "property-change",
                "name": "paused-for-cache",
                "data": true,
            }),
        }))
        .expect("send buffering start");
        tx.send(IpcMessage::Event(IpcEvent {
            observed_at: origin + Duration::from_millis(7),
            value: json!({
                "event": "property-change",
                "name": "paused-for-cache",
                "data": false,
            }),
        }))
        .expect("send buffering end");

        let drain_started = Instant::now();
        let end = drain_until(
            drain_started + Duration::from_millis(40),
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
        let mut telemetry = Telemetry::new(origin, origin + Duration::from_millis(10));
        let (tx, mut rx) = mpsc::unbounded_channel();
        for (elapsed_ms, paused) in [(2, true), (9, false)] {
            tx.send(IpcMessage::Event(IpcEvent {
                observed_at: origin + Duration::from_millis(elapsed_ms),
                value: json!({
                    "event": "property-change",
                    "name": "paused-for-cache",
                    "data": paused,
                }),
            }))
            .expect("queue telemetry before close");
        }
        let clean_eof_at = origin + Duration::from_millis(10);
        tx.send(IpcMessage::CleanEof {
            observed_at: clean_eof_at,
        })
        .expect("queue clean EOF");
        drop(tx);

        let end = drain_until(
            Instant::now() + Duration::from_secs(1),
            origin + Duration::from_millis(10),
            &mut rx,
            &mut telemetry,
            &mut out,
        )
        .await
        .expect("clean observer close is a valid final boundary");

        assert_eq!(end, DrainEnd::CleanEof(clean_eof_at));
        assert_eq!(telemetry.buffering_events, 1);
        assert_eq!(telemetry.buffering_total, Duration::from_millis(7));
        drop(out);
        std::fs::remove_file(path).expect("remove test output");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mpv_reader_distinguishes_clean_eof_from_a_truncated_json_frame() {
        let (mut truncated_writer, truncated_reader) = tokio::io::duplex(256);
        truncated_writer
            .write_all(b"{\"event\":\"playback-restart\"}\n{\"event\"")
            .await
            .expect("write valid line and truncated frame");
        truncated_writer
            .shutdown()
            .await
            .expect("close truncated stream");
        let (truncated_tx, mut truncated_rx) = mpsc::unbounded_channel();
        read_mpv(BufReader::new(truncated_reader), truncated_tx).await;

        assert!(matches!(
            truncated_rx.recv().await,
            Some(IpcMessage::Event(_))
        ));
        match truncated_rx.recv().await {
            Some(IpcMessage::ParseError { message, .. }) => {
                assert!(message.contains("truncated JSON frame"));
            }
            other => panic!("truncated frame must be a parse error, got {other:?}"),
        }
        assert!(truncated_rx.recv().await.is_none());

        let (mut clean_writer, clean_reader) = tokio::io::duplex(256);
        clean_writer
            .write_all(b"{\"event\":\"playback-restart\"}\n")
            .await
            .expect("write complete frame");
        clean_writer.shutdown().await.expect("close clean stream");
        let (clean_tx, mut clean_rx) = mpsc::unbounded_channel();
        read_mpv(BufReader::new(clean_reader), clean_tx).await;

        assert!(matches!(clean_rx.recv().await, Some(IpcMessage::Event(_))));
        assert!(matches!(
            clean_rx.recv().await,
            Some(IpcMessage::CleanEof { .. })
        ));
        assert!(clean_rx.recv().await.is_none());
    }
}
