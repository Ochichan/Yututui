//! Out-of-band playback controller for the TUI performance harness.
//!
//! User actions go through the existing authenticated remote protocol. A second,
//! read-only mpv IPC connection observes `playback-restart`, `paused-for-cache`, and
//! `time-pos` so latency and buffering are measured without injecting terminal keys.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, timeout};
use yututui::remote::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, PushEvent, RemoteCommand,
    ServerFrame, StatusSnapshot, Topic,
};

const SCHEMA: &str = "ytt.tui-perf.control.v1";
const IPC_LINE_CAP: usize = 1024 * 1024;
const MIN_RESUME_PROGRESS_S: f64 = 0.01;
const MPV_EVENT_CHANNEL_CAPACITY: usize = 512;
const CACHE_QUERY_INTERVAL: Duration = Duration::from_secs(1);
const MPV_SUBSCRIPTIONS: &[(u64, &str, bool)] = &[
    (9_001, "paused-for-cache", true),
    (9_002, "time-pos", true),
    (9_003, "cache-on-disk", false),
    (9_007, "demuxer-via-network", false),
    (9_008, "seeking", false),
    (9_009, "duration", false),
    (9_010, "seekable", false),
    (9_011, "partially-seekable", false),
    (9_013, "seekable-ranges", false),
];

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ControllerAction {
    ColdSeek {
        file_generation: String,
        target_s: f64,
    },
    WarmSeek {
        file_generation: String,
        target_s: f64,
    },
    SeekBurst {
        file_generation: String,
        targets_s: Vec<f64>,
        window_ms: u64,
    },
    Recovery {
        file_generation: String,
    },
}

impl ControllerAction {
    fn validate(&self) -> Result<(), String> {
        let (generation, targets): (&str, &[f64]) = match self {
            Self::ColdSeek {
                file_generation,
                target_s,
            }
            | Self::WarmSeek {
                file_generation,
                target_s,
            } => (file_generation, std::slice::from_ref(target_s)),
            Self::SeekBurst {
                file_generation,
                targets_s,
                window_ms,
            } => {
                if !(2..=100).contains(&targets_s.len()) || *window_ms == 0 {
                    return Err(
                        "seek_burst requires 2..=100 targets and a positive window_ms".to_string(),
                    );
                }
                (file_generation, targets_s)
            }
            Self::Recovery { file_generation } => (file_generation, &[]),
        };
        if generation.is_empty() {
            return Err("controller action file_generation must not be empty".to_string());
        }
        if targets
            .iter()
            .any(|target| !target.is_finite() || *target < 0.0)
        {
            return Err("controller action targets must be finite and non-negative".to_string());
        }
        Ok(())
    }
}

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
    actions: Vec<ControllerAction>,
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
        let mut actions: Vec<ControllerAction> = Vec::new();
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
                "--actions-json" => {
                    actions = serde_json::from_str(&next("--actions-json", &mut raw)?)
                        .map_err(|error| format!("invalid --actions-json: {error}"))?;
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
        if !seeks.is_empty() && !actions.is_empty() {
            return Err("--seeks and --actions-json are mutually exclusive".to_string());
        }
        for action in &actions {
            action.validate()?;
        }
        Ok(Self {
            output: output.ok_or_else(|| "--output is required".to_string())?,
            ready_file: ready_file.ok_or_else(|| "--ready-file is required".to_string())?,
            wait: Duration::from_secs_f64(wait_secs),
            observe: Duration::from_secs_f64(observe_secs),
            close_grace: Duration::from_secs_f64(close_grace_secs),
            load,
            seeks,
            actions,
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
       --actions-json JSON           Typed cold/warm/burst action array; excludes --seeks\n\
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

fn scheduled_action_count(actions: usize, pause_hold: Option<Duration>) -> usize {
    actions + usize::from(pause_hold.is_some())
}

fn burst_target_offset(window: Duration, index: usize, target_count: usize) -> Duration {
    if target_count <= 1 {
        return Duration::ZERO;
    }
    let offset_ns = window.as_nanos().saturating_mul(index as u128) / (target_count - 1) as u128;
    Duration::from_nanos(u64::try_from(offset_ns).unwrap_or(u64::MAX))
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
    latest_properties: BTreeMap<String, Value>,
    lifecycle_events: BTreeMap<String, u64>,
    peak_file_cache_bytes: u64,
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
            latest_properties: BTreeMap::new(),
            lifecycle_events: BTreeMap::new(),
            peak_file_cache_bytes: 0,
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
        if let Some(event_type) = event.value.get("event").and_then(Value::as_str) {
            if matches!(
                event_type,
                "start-file" | "file-loaded" | "playback-restart" | "end-file" | "idle"
            ) {
                *self
                    .lifecycle_events
                    .entry(event_type.to_string())
                    .or_default() += 1;
            }
            if event_type == "property-change"
                && let Some(name) = event.value.get("name").and_then(Value::as_str)
            {
                let data = event.value.get("data").cloned().unwrap_or(Value::Null);
                if name == "demuxer-cache-state"
                    && let Some(bytes) = demuxer_cache_file_bytes(&data)
                {
                    self.peak_file_cache_bytes = self.peak_file_cache_bytes.max(bytes);
                }
                self.latest_properties.insert(name.to_string(), data);
            }
        }
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

fn demuxer_cache_file_bytes(value: &Value) -> Option<u64> {
    value.get("file-cache-bytes").and_then(Value::as_u64)
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
    rx: &mut mpsc::Receiver<IpcMessage>,
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
    let cache_query_stream = connect_mpv(&mpv.endpoint, args.wait).await?;
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
    let (tx, mut rx) = mpsc::channel(MPV_EVENT_CHANNEL_CAPACITY);
    tokio::spawn(read_mpv(reader, tx.clone()));
    tokio::spawn(poll_cache_telemetry(cache_query_stream, tx));
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
                json!({}),
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
                json!({}),
            )
            .await?;
        }
    }

    // Seek through the app's public control lane so position_epoch and all owner invariants run.
    let seek_action_count = if args.actions.is_empty() {
        args.seeks.len()
    } else {
        args.actions.len()
    };
    let scheduled_actions = scheduled_action_count(seek_action_count, args.pause_hold);
    for index in 0..seek_action_count {
        if scheduled_actions > 0 && !args.observe.is_zero() {
            let offset = scheduled_offset(args.observe, index + 1, scheduled_actions);
            drain_to_action(origin + offset, &mut rx, &mut telemetry, &mut out).await?;
        }
        if args.actions.is_empty() {
            perform_seek_action(
                &mpv.endpoint,
                "seek",
                "legacy-media-00",
                args.seeks[index],
                args.wait,
                &mut rx,
                &mut telemetry,
                &mut out,
            )
            .await?;
        } else {
            match args.actions[index].clone() {
                ControllerAction::ColdSeek {
                    file_generation,
                    target_s,
                } => {
                    perform_seek_action(
                        &mpv.endpoint,
                        "cold_seek",
                        &file_generation,
                        target_s,
                        args.wait,
                        &mut rx,
                        &mut telemetry,
                        &mut out,
                    )
                    .await?;
                }
                ControllerAction::WarmSeek {
                    file_generation,
                    target_s,
                } => {
                    perform_seek_action(
                        &mpv.endpoint,
                        "warm_seek",
                        &file_generation,
                        target_s,
                        args.wait,
                        &mut rx,
                        &mut telemetry,
                        &mut out,
                    )
                    .await?;
                }
                ControllerAction::SeekBurst {
                    file_generation,
                    targets_s,
                    window_ms,
                } => {
                    perform_seek_burst(
                        SeekBurstArgs {
                            instance: &instance,
                            mpv_endpoint: &mpv.endpoint,
                            file_generation: &file_generation,
                            targets_s: &targets_s,
                            window_ms,
                            wait: args.wait,
                        },
                        SeekBurstContext {
                            rx: &mut rx,
                            telemetry: &mut telemetry,
                            out: &mut out,
                        },
                    )
                    .await?;
                }
                ControllerAction::Recovery { file_generation } => {
                    perform_load(
                        RemoteCommand::ResumeSession,
                        "recovery",
                        args.wait,
                        &mut rx,
                        &mut telemetry,
                        &mut out,
                        json!({
                            "action_kind": "recovery",
                            "file_generation": file_generation,
                        }),
                    )
                    .await?;
                }
            }
        }
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

    // mpv cache properties prove when disk caching changed. The daemon's settings projection
    // independently proves why its managed controller made that decision. Capture it while the
    // owner is still alive; the sampler intentionally closes the owner after the observation.
    let long_form_seek_status = capture_long_form_seek_status(&instance, args.wait).await?;
    write_ndjson(
        &mut out,
        &json!({
            "schema": SCHEMA,
            "kind": "remote_settings_snapshot",
            "elapsed_ns": Instant::now().saturating_duration_since(origin).as_nanos(),
            "long_form_seek_status": &long_form_seek_status,
        }),
    )?;

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
            SummaryArgs {
                args: &args,
                origin,
                summary_at,
                observation_end,
                run_id: &run_id,
                long_form_seek_status: &long_form_seek_status,
                observation_finished_unix_ns: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| format!("system clock before Unix epoch: {e}"))?
                    .as_nanos(),
            },
        ),
    )?;
    out.flush().map_err(|e| format!("flush output: {e}"))
}

async fn write_session_line<T: Serialize>(stream: &mut Stream, value: &T) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| format!("encode remote settings session frame: {error}"))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| format!("write remote settings session frame: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flush remote settings session frame: {error}"))
}

async fn read_session_line(
    reader: &mut BufReader<Stream>,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("timed out reading the remote settings projection".to_string());
    }
    let mut line = Vec::new();
    let outcome = timeout(
        remaining,
        yututui::util::io::read_bounded_line(
            reader,
            &mut line,
            yututui::remote::proto::MAX_ONESHOT_REPLY_BYTES,
        ),
    )
    .await
    .map_err(|_| "timed out reading the remote settings projection".to_string())?
    .map_err(|error| format!("read remote settings projection: {error}"))?;
    match outcome {
        yututui::util::io::BoundedLine::Line => Ok(line),
        yututui::util::io::BoundedLine::Eof => {
            Err("remote owner closed before publishing its settings projection".to_string())
        }
        yututui::util::io::BoundedLine::TooLarge => {
            Err("remote owner published an oversized settings projection".to_string())
        }
    }
}

async fn capture_long_form_seek_status(
    instance: &yututui::remote::proto::InstanceFile,
    wait: Duration,
) -> Result<Value, String> {
    let capability_advertised = instance
        .capabilities
        .iter()
        .any(|capability| capability == yututui::remote::LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY);
    if !capability_advertised {
        return Ok(json!({
            "available": false,
            "capability_advertised": false,
            "requested": null,
            "effective": null,
            "reason": null,
        }));
    }
    if instance.protocol_version < yututui::remote::proto::PROTOCOL_VERSION {
        return Err("long-form seek capability was advertised without protocol v8".to_string());
    }

    let name = instance
        .endpoint
        .as_str()
        .to_fs_name::<GenericFilePath>()
        .map_err(|error| format!("invalid remote settings endpoint: {error}"))?;
    let stream = timeout(wait, Stream::connect(name))
        .await
        .map_err(|_| "timed out connecting for the remote settings projection".to_string())?
        .map_err(|error| format!("connect for remote settings projection: {error}"))?;
    let mut reader = BufReader::new(stream);
    let hello = HelloRequest {
        version: yututui::remote::proto::PROTOCOL_VERSION,
        token: instance.token.clone(),
        hello: HelloBody {
            client: "tui-perf-control".to_string(),
            min_version: yututui::remote::proto::PROTOCOL_VERSION,
        },
    };
    write_session_line(reader.get_mut(), &hello).await?;
    let deadline = Instant::now() + wait;
    let hello_bytes = read_session_line(&mut reader, deadline).await?;
    let ack: HelloAck = serde_json::from_slice(&hello_bytes)
        .map_err(|error| format!("parse remote settings handshake: {error}"))?;
    if !ack.ok || ack.version != yututui::remote::proto::PROTOCOL_VERSION {
        return Err(format!(
            "remote settings handshake rejected: {}",
            ack.reason.as_deref().unwrap_or("bad_version")
        ));
    }

    let subscribe = ClientFrame {
        id: 1,
        request_id: None,
        page_id: None,
        op: ClientOp::Subscribe {
            topics: vec![Topic::Settings],
        },
    };
    write_session_line(reader.get_mut(), &subscribe).await?;
    let mut status = None;
    let mut subscribed = false;
    while status.is_none() || !subscribed {
        let frame_bytes = read_session_line(&mut reader, deadline).await?;
        let frame: ServerFrame = serde_json::from_slice(&frame_bytes)
            .map_err(|error| format!("parse remote settings frame: {error}"))?;
        match frame {
            ServerFrame::Event {
                topic: Topic::Settings,
                event: PushEvent::SettingsSnapshot { model },
                ..
            } => {
                let requested = model.audio.long_form_seek_optimization.ok_or_else(|| {
                    "long-form seek capability omitted requested mode".to_string()
                })?;
                let effective = model.audio.long_form_seek_effective.ok_or_else(|| {
                    "long-form seek capability omitted effective state".to_string()
                })?;
                let reason = model.audio.long_form_seek_reason.ok_or_else(|| {
                    "long-form seek capability omitted decision reason".to_string()
                })?;
                status = Some(json!({
                    "available": true,
                    "capability_advertised": true,
                    "requested": requested,
                    "effective": effective,
                    "reason": reason,
                }));
            }
            ServerFrame::Reply { id: 1, resp } if resp.ok => subscribed = true,
            ServerFrame::Reply { id: 1, resp } => {
                return Err(format!(
                    "remote settings subscription rejected: {}",
                    resp.reason.as_deref().unwrap_or("rejected")
                ));
            }
            ServerFrame::Goodbye { reason } => {
                return Err(format!(
                    "remote settings session ended before its snapshot: {reason}"
                ));
            }
            _ => {
                return Err("remote settings session published an unexpected frame".to_string());
            }
        }
    }
    status.ok_or_else(|| "remote settings projection was not published".to_string())
}

async fn drain_until(
    deadline: Instant,
    minimum_clean_eof: Instant,
    rx: &mut mpsc::Receiver<IpcMessage>,
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
    rx: &mut mpsc::Receiver<IpcMessage>,
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
        let mut descendants = std::collections::BTreeSet::from([app_pid]);
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
        let mut candidates = Vec::new();
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
            candidates.push((process.start_time(), pid, endpoint));
        }
        match select_unique_mpv_candidate(candidates) {
            Ok(Some((pid, endpoint))) => {
                let process = system
                    .process(sysinfo::Pid::from_u32(pid))
                    .ok_or_else(|| format!("selected mpv PID {pid} disappeared"))?;
                return Ok((
                    owner,
                    MpvBinding {
                        process: process_identity(pid, process)?,
                        endpoint,
                    },
                ));
            }
            Ok(None) => {}
            Err(endpoint_candidates) if Instant::now() >= deadline => {
                return Err(format!(
                    "timed out waiting for one exact mpv IPC descendant of ytt PID {app_pid}; \
                     found {endpoint_candidates} endpoint-bearing candidates"
                ));
            }
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for an endpoint-bearing mpv descendant of ytt PID {app_pid}"
            ));
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn select_unique_mpv_candidate(
    mut candidates: Vec<(u64, u32, Option<String>)>,
) -> Result<Option<(u32, String)>, usize> {
    candidates.retain(|(_, _, endpoint)| endpoint.is_some());
    candidates.sort_by(|left, right| {
        (left.0, left.1, left.2.as_deref()).cmp(&(right.0, right.1, right.2.as_deref()))
    });
    match candidates.len() {
        0 => Ok(None),
        1 => {
            let (_, pid, endpoint) = candidates.pop().expect("one candidate");
            Ok(Some((pid, endpoint.expect("endpoint-bearing candidate"))))
        }
        count => Err(count),
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
    for (id, property, _required) in MPV_SUBSCRIPTIONS {
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
    while confirmed.len() < MPV_SUBSCRIPTIONS.len() {
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
                let Some((_id, property, required)) = MPV_SUBSCRIPTIONS
                    .iter()
                    .find(|(id, _property, _required)| *id == request_id)
                else {
                    continue;
                };
                let error = value.get("error").and_then(Value::as_str);
                if *required && error != Some("success") {
                    return Err(format!(
                        "required mpv subscription {request_id} ({property}) failed: {}",
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

async fn query_mpv_property<R>(
    reader: &mut BufReader<R>,
    request_id: u64,
    property: &str,
    required: bool,
    wait: Duration,
) -> Result<Option<Value>, String>
where
    R: AsyncRead + AsyncWrite + Unpin,
{
    let mut payload = serde_json::to_vec(&json!({
        "command": ["get_property", property],
        "request_id": request_id,
    }))
    .map_err(|error| format!("encode mpv {property} query: {error}"))?;
    payload.push(b'\n');
    reader
        .get_mut()
        .write_all(&payload)
        .await
        .map_err(|error| format!("write mpv {property} query: {error}"))?;
    reader
        .get_mut()
        .flush()
        .await
        .map_err(|error| format!("flush mpv {property} query: {error}"))?;
    let deadline = Instant::now() + wait;
    let mut line = Vec::new();
    loop {
        line.clear();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for mpv {property} query"));
        }
        let outcome = timeout(
            remaining,
            yututui::util::io::read_bounded_line(reader, &mut line, IPC_LINE_CAP),
        )
        .await
        .map_err(|_| format!("timed out waiting for mpv {property} query"))?
        .map_err(|error| format!("read mpv {property} query: {error}"))?;
        match outcome {
            yututui::util::io::BoundedLine::Eof => {
                return Err(format!(
                    "mpv cache query connection closed while reading {property}"
                ));
            }
            yututui::util::io::BoundedLine::TooLarge => {
                return Err(format!("mpv {property} query returned an oversized frame"));
            }
            yututui::util::io::BoundedLine::Line => {}
        }
        let response: Value = serde_json::from_slice(&line)
            .map_err(|error| format!("parse mpv {property} query: {error}"))?;
        match response.get("request_id") {
            Some(actual) if actual.as_u64() == Some(request_id) => {}
            None if response.get("event").and_then(Value::as_str).is_some() => continue,
            actual => {
                return Err(format!(
                    "mpv {property} query returned an uncorrelated response \
                     (expected request_id {request_id}, got {})",
                    actual.unwrap_or(&Value::Null)
                ));
            }
        }
        if response.get("error").and_then(Value::as_str) == Some("success") {
            return Ok(Some(response.get("data").cloned().unwrap_or(Value::Null)));
        }
        if required {
            return Err(format!(
                "required mpv {property} query failed: {}",
                response.get("error").unwrap_or(&Value::Null)
            ));
        }
        return Ok(None);
    }
}

fn narrow_demuxer_cache_state(state: Value) -> Result<Value, String> {
    let Value::Object(state) = state else {
        return Err("mpv demuxer-cache-state query did not return an object".to_string());
    };
    let file_cache_bytes = state
        .get("file-cache-bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| "mpv demuxer-cache-state omitted file-cache-bytes".to_string())?;
    let mut narrow = serde_json::Map::from_iter([(
        "file-cache-bytes".to_string(),
        Value::from(file_cache_bytes),
    )]);
    if let Some(raw_input_rate) = state.get("raw-input-rate").and_then(Value::as_u64) {
        narrow.insert("raw-input-rate".to_string(), Value::from(raw_input_rate));
    }
    Ok(Value::Object(narrow))
}

fn queried_property_event(
    property: &str,
    data: Value,
    query_kind: &str,
    sequence: u64,
) -> IpcEvent {
    IpcEvent {
        observed_at: Instant::now(),
        value: json!({
            "event": "property-change",
            "name": property,
            "data": data,
            "harness_query": {
                "kind": query_kind,
                "sequence": sequence,
                "full_state_recorded": false,
            },
        }),
    }
}

async fn poll_cache_telemetry(stream: Stream, tx: mpsc::Sender<IpcMessage>) {
    let mut reader = BufReader::new(stream);
    let mut sequence = 0u64;
    let mut next_query = Instant::now();
    loop {
        if Instant::now() < next_query {
            sleep_until(tokio::time::Instant::from_std(next_query)).await;
        }
        sequence = sequence.saturating_add(1);
        let result: Result<Vec<IpcEvent>, String> = async {
            let cache_speed = query_mpv_property(
                &mut reader,
                91_000,
                "cache-speed",
                false,
                CACHE_QUERY_INTERVAL,
            )
            .await?;
            let cache_on_disk = query_mpv_property(
                &mut reader,
                91_001,
                "cache-on-disk",
                true,
                CACHE_QUERY_INTERVAL,
            )
            .await?;
            let mut events = Vec::new();
            if let Some(value) = cache_speed {
                events.push(queried_property_event(
                    "cache-speed",
                    value,
                    "periodic_rate_poll",
                    sequence,
                ));
            }
            if cache_on_disk.and_then(|value| value.as_bool()) == Some(true) {
                let state = query_mpv_property(
                    &mut reader,
                    91_002,
                    "demuxer-cache-state",
                    true,
                    CACHE_QUERY_INTERVAL,
                )
                .await?
                .ok_or_else(|| "required demuxer-cache-state query returned no data".to_string())?;
                events.push(queried_property_event(
                    "demuxer-cache-state",
                    narrow_demuxer_cache_state(state)?,
                    "active_cache_poll",
                    sequence,
                ));
            }
            Ok(events)
        }
        .await;
        match result {
            Ok(events) => {
                for event in events {
                    if tx.send(IpcMessage::Event(event)).await.is_err() {
                        return;
                    }
                }
            }
            // The subscribed observer connection owns the run's clean-EOF boundary. This
            // auxiliary query connection must never end coverage early during owner shutdown.
            Err(message) if cache_query_transport_closed(&message) => return,
            Err(message) => {
                let _ = tx
                    .send(IpcMessage::IoError {
                        observed_at: Instant::now(),
                        message,
                    })
                    .await;
                return;
            }
        }
        next_query += CACHE_QUERY_INTERVAL;
        if next_query < Instant::now() {
            next_query = Instant::now() + CACHE_QUERY_INTERVAL;
        }
    }
}

fn cache_query_transport_closed(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    [
        "connection closed",
        "broken pipe",
        "connection reset",
        "not connected",
        "forcibly closed",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

async fn capture_pre_seek_cache_snapshot(
    endpoint: &str,
    wait: Duration,
    rx: &mut mpsc::Receiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    drain_pending_mpv(rx, telemetry, out)?;
    let stream = connect_mpv(endpoint, wait).await?;
    let mut reader = BufReader::new(stream);
    let state = query_mpv_property(&mut reader, 92_000, "demuxer-cache-state", true, wait)
        .await?
        .ok_or_else(|| "pre-seek demuxer-cache-state query returned no data".to_string())?;
    let cache_speed = query_mpv_property(&mut reader, 92_001, "cache-speed", false, wait).await?;
    let mut events = vec![queried_property_event(
        "demuxer-cache-state",
        narrow_demuxer_cache_state(state)?,
        "pre_seek_snapshot",
        1,
    )];
    if let Some(value) = cache_speed {
        events.push(queried_property_event(
            "cache-speed",
            value,
            "pre_seek_snapshot",
            1,
        ));
    }
    while let Ok(message) = rx.try_recv() {
        match message {
            IpcMessage::Event(event) => events.push(event),
            terminal => return Err(ipc_terminal_error(terminal)),
        }
    }
    events.sort_by_key(|event| event.observed_at);
    for event in events {
        telemetry.record(&event, out)?;
    }
    Ok(())
}

async fn read_mpv<R>(mut reader: BufReader<R>, tx: mpsc::Sender<IpcMessage>)
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
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx
                            .send(IpcMessage::ParseError {
                                observed_at: Instant::now(),
                                message: error.to_string(),
                            })
                            .await;
                        return;
                    }
                }
            }
            Ok(yututui::util::io::BoundedLine::Eof) => {
                let observed_at = Instant::now();
                if line.is_empty() {
                    let _ = tx.send(IpcMessage::CleanEof { observed_at }).await;
                } else {
                    let _ = tx
                        .send(IpcMessage::ParseError {
                            observed_at,
                            message: format!(
                                "mpv IPC closed with a truncated JSON frame ({} bytes)",
                                line.len()
                            ),
                        })
                        .await;
                }
                return;
            }
            Ok(yututui::util::io::BoundedLine::TooLarge) => {
                let _ = tx
                    .send(IpcMessage::TooLarge {
                        observed_at: Instant::now(),
                    })
                    .await;
                return;
            }
            Err(error) => {
                let _ = tx
                    .send(IpcMessage::IoError {
                        observed_at: Instant::now(),
                        message: error.to_string(),
                    })
                    .await;
                return;
            }
        }
    }
}

async fn perform_load(
    command: RemoteCommand,
    name: &str,
    wait: Duration,
    rx: &mut mpsc::Receiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
    detail: Value,
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
        detail,
    )
}

#[allow(clippy::too_many_arguments)]
async fn perform_seek_action(
    mpv_endpoint: &str,
    operation_name: &str,
    file_generation: &str,
    target_s: f64,
    wait: Duration,
    rx: &mut mpsc::Receiver<IpcMessage>,
    telemetry: &mut Telemetry,
    out: &mut BufWriter<File>,
) -> Result<(), String> {
    capture_pre_seek_cache_snapshot(mpv_endpoint, wait, rx, telemetry, out).await?;
    let started = Instant::now();
    send_remote(RemoteCommand::SeekTo {
        ms: (target_s * 1_000.0).round() as u64,
    })
    .await?;
    let (restart, completed, observed_target_s) =
        wait_for_seek(wait, rx, telemetry, out, started, target_s).await?;
    operation(
        out,
        telemetry.origin,
        operation_name,
        started..completed.observed_at,
        "mpv_event",
        Some(&completed),
        json!({
            "action_kind": operation_name,
            "file_generation": file_generation,
            "target_s": target_s,
            "observed_target_s": observed_target_s,
            "playback_restart_elapsed_ns": restart
                .observed_at
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "target_tolerance_s": 2.0,
        }),
    )
}

struct SeekBurstArgs<'a> {
    instance: &'a yututui::remote::proto::InstanceFile,
    mpv_endpoint: &'a str,
    file_generation: &'a str,
    targets_s: &'a [f64],
    window_ms: u64,
    wait: Duration,
}

struct SeekBurstContext<'a> {
    rx: &'a mut mpsc::Receiver<IpcMessage>,
    telemetry: &'a mut Telemetry,
    out: &'a mut BufWriter<File>,
}

async fn perform_seek_burst(
    args: SeekBurstArgs<'_>,
    context: SeekBurstContext<'_>,
) -> Result<(), String> {
    let SeekBurstArgs {
        instance,
        mpv_endpoint,
        file_generation,
        targets_s,
        window_ms,
        wait,
    } = args;
    let SeekBurstContext { rx, telemetry, out } = context;
    let final_target = *targets_s
        .last()
        .ok_or_else(|| "seek burst has no final target".to_string())?;
    capture_pre_seek_cache_snapshot(mpv_endpoint, wait, rx, telemetry, out).await?;

    if instance.protocol_version < yututui::remote::proto::PROTOCOL_VERSION {
        return Err("ordered seek bursts require the remote session protocol".to_string());
    }
    let name = instance
        .endpoint
        .as_str()
        .to_fs_name::<GenericFilePath>()
        .map_err(|error| format!("invalid remote burst endpoint: {error}"))?;
    let stream = timeout(wait, Stream::connect(name))
        .await
        .map_err(|_| "timed out connecting the ordered remote burst session".to_string())?
        .map_err(|error| format!("connect ordered remote burst session: {error}"))?;
    let mut reader = BufReader::new(stream);
    write_session_line(
        reader.get_mut(),
        &HelloRequest {
            version: yututui::remote::proto::PROTOCOL_VERSION,
            token: instance.token.clone(),
            hello: HelloBody {
                client: "tui-perf-burst".to_string(),
                min_version: yututui::remote::proto::PROTOCOL_VERSION,
            },
        },
    )
    .await?;
    let handshake_deadline = Instant::now() + wait;
    let hello_bytes = read_session_line(&mut reader, handshake_deadline).await?;
    let ack: HelloAck = serde_json::from_slice(&hello_bytes)
        .map_err(|error| format!("parse ordered remote burst handshake: {error}"))?;
    if !ack.ok || ack.version != yututui::remote::proto::PROTOCOL_VERSION {
        return Err(format!(
            "ordered remote burst handshake rejected: {}",
            ack.reason.as_deref().unwrap_or("bad_version")
        ));
    }

    let burst_started = Instant::now();
    let window = Duration::from_millis(window_ms);
    let mut final_dispatched = None;
    let mut final_write_completed = None;
    let mut max_schedule_lateness = Duration::ZERO;
    for (index, target_s) in targets_s.iter().copied().enumerate() {
        let scheduled_at = burst_started + burst_target_offset(window, index, targets_s.len());
        if Instant::now() < scheduled_at {
            sleep_until(tokio::time::Instant::from_std(scheduled_at)).await;
        }
        let dispatched_at = Instant::now();
        max_schedule_lateness =
            max_schedule_lateness.max(dispatched_at.saturating_duration_since(scheduled_at));
        if index + 1 == targets_s.len() {
            final_dispatched = Some(dispatched_at);
        }
        let id = u64::try_from(index + 1)
            .map_err(|_| "seek burst contains too many targets".to_string())?;
        write_session_line(
            reader.get_mut(),
            &ClientFrame {
                id,
                request_id: None,
                page_id: None,
                op: ClientOp::Command(RemoteCommand::SeekTo {
                    ms: (target_s * 1_000.0).round() as u64,
                }),
            },
        )
        .await?;
        if index + 1 == targets_s.len() {
            final_write_completed = Some(Instant::now());
        }
    }

    let reply_deadline = Instant::now() + wait;
    let mut first_owner_reply = None;
    let mut final_admitted = None;
    for expected_index in 0..targets_s.len() {
        let frame_bytes = read_session_line(&mut reader, reply_deadline).await?;
        let frame: ServerFrame = serde_json::from_slice(&frame_bytes)
            .map_err(|error| format!("parse ordered remote burst reply: {error}"))?;
        let expected_id = u64::try_from(expected_index + 1)
            .map_err(|_| "seek burst contains too many targets".to_string())?;
        match frame {
            ServerFrame::Reply { id, resp } if id == expected_id && resp.ok => {
                let observed_at = Instant::now();
                first_owner_reply.get_or_insert(observed_at);
                if expected_index + 1 == targets_s.len() {
                    final_admitted = Some(observed_at);
                }
            }
            ServerFrame::Reply { id, resp } if id == expected_id => {
                return Err(format!(
                    "ordered seek burst target {expected_id} rejected: {}",
                    resp.reason.as_deref().unwrap_or("rejected")
                ));
            }
            ServerFrame::Reply { id, .. } => {
                return Err(format!(
                    "ordered seek burst reply {id} arrived before expected reply {expected_id}"
                ));
            }
            ServerFrame::Goodbye { reason } => {
                return Err(format!("ordered remote burst session ended: {reason}"));
            }
            _ => {
                return Err(
                    "ordered remote burst session published an unexpected frame".to_string()
                );
            }
        }
    }
    let final_dispatched = final_dispatched.expect("non-empty burst checked above");
    let final_write_completed = final_write_completed.expect("non-empty burst checked above");
    let first_owner_reply = first_owner_reply
        .ok_or_else(|| "seek burst did not receive an owner command response".to_string())?;
    let final_admitted = final_admitted.ok_or_else(|| {
        "seek burst final target did not receive an admission response".to_string()
    })?;
    let (restart, completed, observed_target_s) =
        wait_for_seek(wait, rx, telemetry, out, final_dispatched, final_target).await?;
    operation(
        out,
        telemetry.origin,
        "seek_burst",
        final_dispatched..completed.observed_at,
        "mpv_event",
        Some(&completed),
        json!({
            "action_kind": "seek_burst",
            "file_generation": file_generation,
            "targets_s": targets_s,
            "target_s": final_target,
            "window_ms": window_ms,
            "submitted_without_intermediate_wait": true,
            "transport_kind": "ordered_remote_session_v8",
            "dispatch_scope": "client_session_write",
            "reply_order_proven": true,
            "owner_reply_semantics": "owner_command_response_not_wire_dispatch",
            "owner_reply_window_ship_evidence": false,
            "schedule_kind": "absolute_monotonic_deadlines_v1",
            "burst_started_elapsed_ns": burst_started
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "actual_dispatch_window_ns": final_dispatched
                .saturating_duration_since(burst_started)
                .as_nanos(),
            "max_schedule_lateness_ns": max_schedule_lateness.as_nanos(),
            "final_target_dispatched_elapsed_ns": final_dispatched
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "final_target_write_completed_elapsed_ns": final_write_completed
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "final_target_admitted_elapsed_ns": final_admitted
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "first_owner_reply_elapsed_ns": first_owner_reply
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "owner_reply_window_ns": final_admitted
                .saturating_duration_since(first_owner_reply)
                .as_nanos(),
            "latency_anchor": "final_target_dispatch",
            "observed_target_s": observed_target_s,
            "playback_restart_elapsed_ns": restart
                .observed_at
                .saturating_duration_since(telemetry.origin)
                .as_nanos(),
            "target_tolerance_s": 2.0,
        }),
    )
}

async fn wait_for_mpv(
    wait: Duration,
    rx: &mut mpsc::Receiver<IpcMessage>,
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
    rx: &mut mpsc::Receiver<IpcMessage>,
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

fn source_rate_bound_bps() -> Option<u64> {
    std::env::var("YTM_PERF_SOURCE_RATE_BOUND_BPS")
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
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
        "subscription_contract": MPV_SUBSCRIPTIONS.iter().map(|(id, property, required)| {
            json!({"id": id, "property": property, "required": required})
        }).collect::<Vec<_>>(),
        "cache_query_contract": {
            "policy": "pre_seek_plus_active_low_rate_v1",
            "interval_ms": CACHE_QUERY_INTERVAL.as_millis(),
            "cache_speed_periodic": true,
            "demuxer_cache_state_only_pre_seek_or_disk_active": true,
            "full_demuxer_cache_state_recorded": false,
            "recorded_state_members": ["file-cache-bytes", "raw-input-rate"],
            "event_channel_capacity": MPV_EVENT_CHANNEL_CAPACITY,
        },
        "source_rate_bound_bps": source_rate_bound_bps(),
        "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pause_policy": if args.pause_hold.is_some() { "pause-resume" } else { "none" },
        "pause_hold_ms": args.pause_hold.map(|hold| hold.as_millis()),
        "typed_actions": !args.actions.is_empty(),
        "seek_action_count": if args.actions.is_empty() {
            args.seeks.len()
        } else {
            args.actions.len()
        },
    })
}

struct SummaryArgs<'a> {
    args: &'a Args,
    origin: Instant,
    summary_at: Instant,
    observation_end: DrainEnd,
    run_id: &'a str,
    long_form_seek_status: &'a Value,
    observation_finished_unix_ns: u128,
}

fn summary_record(telemetry: &Telemetry, args: SummaryArgs<'_>) -> Value {
    let SummaryArgs {
        args,
        origin,
        summary_at,
        observation_end,
        run_id,
        long_form_seek_status,
        observation_finished_unix_ns,
    } = args;
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
        "latest_properties": telemetry.latest_properties,
        "lifecycle_events": telemetry.lifecycle_events,
        "peak_file_cache_bytes": telemetry.peak_file_cache_bytes,
        "long_form_seek_status": long_form_seek_status,
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
        "cache_query_contract": {
            "policy": "pre_seek_plus_active_low_rate_v1",
            "interval_ms": CACHE_QUERY_INTERVAL.as_millis(),
            "cache_speed_periodic": true,
            "demuxer_cache_state_only_pre_seek_or_disk_active": true,
            "full_demuxer_cache_state_recorded": false,
            "recorded_state_members": ["file-cache-bytes", "raw-input-rate"],
            "event_channel_capacity": MPV_EVENT_CHANNEL_CAPACITY,
        },
        "observation_started_unix_ns": observation_started_unix_ns,
        "source_rate_bound_bps": source_rate_bound_bps(),
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
    fn ship_action_bounds_accept_recovery_and_reject_oversized_bursts() {
        ControllerAction::Recovery {
            file_generation: "media-02".to_string(),
        }
        .validate()
        .expect("a non-empty recovery generation is valid");
        ControllerAction::SeekBurst {
            file_generation: "media-05".to_string(),
            targets_s: vec![1.0; 20],
            window_ms: 500,
        }
        .validate()
        .expect("the minimum ship burst is valid");
        let oversized = ControllerAction::SeekBurst {
            file_generation: "media-05".to_string(),
            targets_s: vec![1.0; 101],
            window_ms: 500,
        };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn mpv_discovery_ignores_endpointless_candidates_and_fails_closed_on_ambiguity() {
        let selected = select_unique_mpv_candidate(vec![
            (9, 90, None),
            (8, 80, Some("/tmp/exact.sock".to_string())),
        ])
        .expect("one endpoint-bearing candidate")
        .expect("candidate selected");
        assert_eq!(selected, (80, "/tmp/exact.sock".to_string()));

        let ambiguity = select_unique_mpv_candidate(vec![
            (10, 100, Some("/tmp/newer.sock".to_string())),
            (8, 80, Some("/tmp/older.sock".to_string())),
            (9, 90, None),
        ])
        .expect_err("multiple exact candidates must not be selected by iteration order");
        assert_eq!(ambiguity, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mpv_property_query_skips_events_before_its_correlated_response() {
        let (stream, mut peer) = tokio::io::duplex(4_096);
        peer.write_all(
            b"{\"event\":\"start-file\",\"playlist_entry_id\":2}\n\
              {\"event\":\"file-loaded\"}\n\
              {\"data\":1234,\"request_id\":91000,\"error\":\"success\"}\n",
        )
        .await
        .expect("queue interleaved mpv frames");
        let mut reader = BufReader::new(stream);

        let value = query_mpv_property(
            &mut reader,
            91_000,
            "cache-speed",
            false,
            Duration::from_secs(1),
        )
        .await
        .expect("unsolicited events must not consume the correlated response");

        assert_eq!(value, Some(json!(1234)));
        let mut peer = BufReader::new(peer);
        let mut command = Vec::new();
        assert!(matches!(
            yututui::util::io::read_bounded_line(&mut peer, &mut command, IPC_LINE_CAP)
                .await
                .expect("read property query command"),
            yututui::util::io::BoundedLine::Line
        ));
        assert_eq!(
            serde_json::from_slice::<Value>(&command).expect("parse property query command"),
            json!({
                "command": ["get_property", "cache-speed"],
                "request_id": 91_000,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mpv_property_query_still_rejects_a_different_request_id() {
        let (stream, mut peer) = tokio::io::duplex(1_024);
        peer.write_all(
            b"{\"event\":\"file-loaded\"}\n\
              {\"data\":1234,\"request_id\":91001,\"error\":\"success\"}\n",
        )
        .await
        .expect("queue mismatched mpv response");
        let mut reader = BufReader::new(stream);

        let error = query_mpv_property(
            &mut reader,
            91_000,
            "cache-speed",
            false,
            Duration::from_secs(1),
        )
        .await
        .expect_err("a response for another command must remain fail-closed");

        assert!(error.contains("uncorrelated response"));
        assert!(error.contains("expected request_id 91000"));
        assert!(error.contains("got 91001"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mpv_property_query_events_do_not_extend_the_absolute_deadline() {
        let (stream, mut peer) = tokio::io::duplex(1_024);
        let (event_sent, event_observed) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            sleep(Duration::from_millis(150)).await;
            peer.write_all(b"{\"event\":\"file-loaded\",\"id\":91000}\n")
                .await
                .expect("write interleaved event");
            let _ = event_sent.send(());
            sleep(Duration::from_millis(500)).await;
        });
        let mut reader = BufReader::new(stream);
        let started = Instant::now();

        let error = query_mpv_property(
            &mut reader,
            91_000,
            "cache-speed",
            false,
            Duration::from_millis(250),
        )
        .await
        .expect_err("events without a correlated response must time out");
        let elapsed = started.elapsed();
        event_observed
            .await
            .expect("the event must arrive before the query deadline");
        writer.abort();

        assert!(error.contains("timed out waiting for mpv cache-speed query"));
        assert!(elapsed >= Duration::from_millis(150));
        assert!(
            elapsed < Duration::from_millis(325),
            "the interleaved event reset the absolute deadline: {elapsed:?}"
        );
    }

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
            actions: Vec::new(),
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
            actions: Vec::new(),
            pause_hold: None,
        };
        let summary = summary_record(
            &telemetry,
            SummaryArgs {
                args: &args,
                origin,
                summary_at,
                observation_end: DrainEnd::CleanEof(summary_at),
                run_id: "self-test-run",
                long_form_seek_status: &json!({
                    "available": true,
                    "capability_advertised": true,
                    "requested": "auto",
                    "effective": "disk_active",
                    "reason": "auto_uncached_seek",
                }),
                observation_finished_unix_ns: 456,
            },
        );

        assert_eq!(summary["elapsed_ns"], 7_000_000u64);
        assert_eq!(summary["buffering_events"], 1);
        assert_eq!(summary["buffering_ms"], 3);
        assert_eq!(summary["buffering_cutoff_ns"], 5_000_000u64);
        assert_eq!(summary["observation_end"], "mpv_ipc_closed");
        assert_eq!(summary["run_id"], "self-test-run");
        assert_eq!(summary["observation_finished_unix_ns"], 456);
        assert_eq!(
            summary["long_form_seek_status"]["reason"],
            "auto_uncached_seek"
        );
    }

    #[test]
    fn file_cache_peak_is_extracted_from_demuxer_cache_state() {
        assert_eq!(
            demuxer_cache_file_bytes(&json!({
                "raw-input-rate": 4_096,
                "file-cache-bytes": 12_345,
            })),
            Some(12_345)
        );
        assert_eq!(demuxer_cache_file_bytes(&json!(12_345)), None);
        assert_eq!(
            demuxer_cache_file_bytes(&json!({"file-cache-bytes": -1})),
            None
        );
    }

    #[test]
    fn scheduled_actions_are_spread_across_the_observation_window() {
        let window = Duration::from_secs(70);
        assert_eq!(scheduled_offset(window, 1, 6), Duration::from_secs(10));
        assert_eq!(scheduled_offset(window, 3, 6), Duration::from_secs(30));
        assert_eq!(scheduled_offset(window, 6, 6), Duration::from_secs(60));
    }

    #[test]
    fn burst_targets_use_absolute_deadlines_and_end_at_the_declared_window() {
        let window = Duration::from_millis(500);
        let offsets = (0..20)
            .map(|index| burst_target_offset(window, index, 20))
            .collect::<Vec<_>>();
        assert_eq!(offsets[0], Duration::ZERO);
        assert_eq!(offsets[19], window);
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        let simulated_rpc_completion = offsets[0] + Duration::from_millis(200);
        assert!(offsets[1] < simulated_rpc_completion);
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
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(IpcMessage::Event(IpcEvent {
            observed_at: origin + Duration::from_millis(2),
            value: json!({
                "event": "property-change",
                "name": "paused-for-cache",
                "data": true,
            }),
        }))
        .expect("send buffering start");
        tx.try_send(IpcMessage::Event(IpcEvent {
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
        let (tx, mut rx) = mpsc::channel(8);
        for (elapsed_ms, paused) in [(2, true), (9, false)] {
            tx.try_send(IpcMessage::Event(IpcEvent {
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
        tx.try_send(IpcMessage::CleanEof {
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
        let (truncated_tx, mut truncated_rx) = mpsc::channel(8);
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
        let (clean_tx, mut clean_rx) = mpsc::channel(8);
        read_mpv(BufReader::new(clean_reader), clean_tx).await;

        assert!(matches!(clean_rx.recv().await, Some(IpcMessage::Event(_))));
        assert!(matches!(
            clean_rx.recv().await,
            Some(IpcMessage::CleanEof { .. })
        ));
        assert!(clean_rx.recv().await.is_none());
    }
}
