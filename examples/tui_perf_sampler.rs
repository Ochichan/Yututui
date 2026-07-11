//! Cross-platform process-tree sampler used by `scripts/tui-perf.{sh,ps1}`.
//!
//! The sampler must itself run in an interactive terminal (tmux on Unix, a local
//! ConPTY/console on Windows). It launches exactly one `ytt`, follows only that PID's
//! descendants, and writes NDJSON outside the measured terminal. No process-name-wide
//! cleanup is ever performed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

const SCHEMA: &str = "ytt.tui-perf.samples.v1";

#[derive(Debug)]
struct Args {
    output: PathBuf,
    pid_file: Option<PathBuf>,
    binary: PathBuf,
    child_args: Vec<String>,
    warmup: Duration,
    duration: Duration,
    interval: Duration,
    require_silent_mpv: bool,
    keep_running: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut output = None;
        let mut pid_file = None;
        let mut binary = None;
        let mut warmup_secs = 0.0;
        let mut duration_secs = 60.0;
        let mut interval_ms = 1_000u64;
        let mut require_silent_mpv = false;
        let mut keep_running = false;
        let mut child_args = Vec::new();
        let mut raw = std::env::args().skip(1).peekable();

        while let Some(arg) = raw.next() {
            if arg == "--" {
                child_args.extend(raw);
                break;
            }
            let value = |name: &str,
                         it: &mut std::iter::Peekable<std::iter::Skip<std::env::Args>>|
             -> Result<String, String> {
                it.next().ok_or_else(|| format!("{name} requires a value"))
            };
            match arg.as_str() {
                "--output" => output = Some(PathBuf::from(value("--output", &mut raw)?)),
                "--pid-file" => pid_file = Some(PathBuf::from(value("--pid-file", &mut raw)?)),
                "--binary" => binary = Some(PathBuf::from(value("--binary", &mut raw)?)),
                "--warmup-secs" => {
                    warmup_secs =
                        parse_nonnegative_f64("--warmup-secs", &value("--warmup-secs", &mut raw)?)?;
                }
                "--duration-secs" => {
                    duration_secs = parse_positive_f64(
                        "--duration-secs",
                        &value("--duration-secs", &mut raw)?,
                    )?;
                }
                "--interval-ms" => {
                    interval_ms = value("--interval-ms", &mut raw)?
                        .parse::<u64>()
                        .ok()
                        .filter(|v| *v >= 100)
                        .ok_or_else(|| "--interval-ms must be an integer >= 100".to_string())?;
                }
                "--require-silent-mpv" => require_silent_mpv = true,
                "--keep-running" => keep_running = true,
                "-h" | "--help" => return Err(usage().to_string()),
                other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
            }
        }

        Ok(Self {
            output: output.ok_or_else(|| "--output is required".to_string())?,
            pid_file,
            binary: binary.ok_or_else(|| "--binary is required".to_string())?,
            child_args,
            warmup: Duration::from_secs_f64(warmup_secs),
            duration: Duration::from_secs_f64(duration_secs),
            interval: Duration::from_millis(interval_ms),
            require_silent_mpv,
            keep_running,
        })
    }
}

fn usage() -> &'static str {
    "Usage: tui_perf_sampler --output FILE --binary YTT [options] [-- YTT_ARGS...]\n\
     Options:\n\
       --pid-file FILE          Write the launched ytt PID atomically\n\
       --warmup-secs N          Warm-up samples excluded from the summary (default 0)\n\
       --duration-secs N        Measured duration (default 60)\n\
       --interval-ms N          Sampling interval, at least 100 ms (default 1000)\n\
       --require-silent-mpv     Fail unless effective mpv argv has ao=null and volume=0\n\
       --keep-running           Do not remote-quit/terminate ytt after sampling"
}

fn parse_nonnegative_f64(name: &str, raw: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| format!("{name} must be a finite non-negative number"))
}

fn parse_positive_f64(name: &str, raw: &str) -> Result<f64, String> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| format!("{name} must be a finite positive number"))
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct RoleSample {
    processes: usize,
    cpu_percent: f64,
    rss_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ProcessSample {
    pid: u32,
    parent_pid: Option<u32>,
    role: &'static str,
    name: String,
    start_time_unix_s: u64,
    accumulated_cpu_ms: u64,
    cpu_percent: f64,
    rss_bytes: u64,
    command: Vec<String>,
}

/// The exact mpv descendant identity retained for cleanup after ytt exits. `start_time_unix_s`
/// protects against ordinary PID reuse, while the run-unique IPC argv protects platforms whose
/// process start time is only reported at second resolution. Cleanup never falls back to a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MpvIdentity {
    pid: u32,
    start_time_unix_s: u64,
    input_ipc_server_argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct MeasuredMpvProof {
    samples: u64,
    samples_with_mpv: u64,
    samples_all_silent: u64,
    samples_all_cleanup_identified: u64,
}

impl MeasuredMpvProof {
    fn observe(&mut self, mpv_silence: &[bool], cleanup_identities: usize) {
        self.samples += 1;
        if !mpv_silence.is_empty() {
            self.samples_with_mpv += 1;
            if mpv_silence.iter().all(|silent| *silent) {
                self.samples_all_silent += 1;
            }
            if cleanup_identities == mpv_silence.len() {
                self.samples_all_cleanup_identified += 1;
            }
        }
    }

    fn proven(self) -> bool {
        self.samples > 0
            && self.samples_with_mpv == self.samples
            && self.samples_all_silent == self.samples
    }

    fn cleanup_identities_proven(self) -> bool {
        self.samples > 0 && self.samples_all_cleanup_identified == self.samples
    }
}

#[derive(Clone, Copy)]
struct CpuPoint {
    start_time: u64,
    accumulated_ms: u64,
    observed_at: Instant,
}

#[derive(Clone, Copy, Default)]
struct Aggregate {
    samples: u64,
    cpu_sum: f64,
    rss_sum: u128,
    rss_peak: u64,
}

impl Aggregate {
    fn push(&mut self, value: RoleSample) {
        self.samples += 1;
        self.cpu_sum += value.cpu_percent;
        self.rss_sum += u128::from(value.rss_bytes);
        self.rss_peak = self.rss_peak.max(value.rss_bytes);
    }

    fn json(&self) -> serde_json::Value {
        let divisor = self.samples.max(1) as f64;
        serde_json::json!({
            "samples": self.samples,
            "mean_cpu_percent": self.cpu_sum / divisor,
            "mean_rss_bytes": (self.rss_sum / u128::from(self.samples.max(1))) as u64,
            "peak_rss_bytes": self.rss_peak,
        })
    }
}

fn main() {
    let code = match Args::parse().and_then(run) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("tui_perf_sampler: {message}");
            2
        }
    };
    std::process::exit(code);
}

fn run(args: Args) -> Result<(), String> {
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }
    let file =
        File::create(&args.output).map_err(|e| format!("create {}: {e}", args.output.display()))?;
    let mut out = BufWriter::new(file);

    let binary_hash = sha256_file(&args.binary)?;
    let mut child = Command::new(&args.binary)
        .args(&args.child_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("launch {}: {e}", args.binary.display()))?;
    let root_pid = child.id();

    if let Some(path) = &args.pid_file {
        write_pid_file(path, root_pid)?;
    }

    let mut last_mpv_identities = Vec::new();
    let sample_result = sample_process_tree(
        &args,
        root_pid,
        binary_hash,
        &mut child,
        &mut out,
        &mut last_mpv_identities,
    );
    let cleanup_result = if args.keep_running {
        Ok(())
    } else {
        shutdown_child(
            &args.binary,
            &mut child,
            &last_mpv_identities,
            args.require_silent_mpv,
        )
    };
    let result = match (sample_result, cleanup_result) {
        (Err(sample), Err(cleanup)) => Err(format!("{sample}; cleanup also failed: {cleanup}")),
        (Err(sample), Ok(())) => Err(sample),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    };
    if let Err(message) = &result {
        let _ = write_ndjson(
            &mut out,
            &serde_json::json!({
                "schema": SCHEMA,
                "kind": "error",
                "root_pid": root_pid,
                "message": message,
            }),
        );
    }
    let _ = out.flush();
    result
}

fn sample_process_tree(
    args: &Args,
    root_pid: u32,
    binary_hash: String,
    child: &mut Child,
    out: &mut BufWriter<File>,
    last_mpv_identities: &mut Vec<MpvIdentity>,
) -> Result<(), String> {
    let started = Instant::now();
    let deadline = args.warmup + args.duration;
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet)
        .without_tasks();
    let mut identities: HashMap<u32, u64> = HashMap::new();
    let mut cpu_points: HashMap<u32, CpuPoint> = HashMap::new();
    let mut measured: BTreeMap<&'static str, Aggregate> = BTreeMap::new();
    let mut measured_mpv_proof = MeasuredMpvProof::default();
    let mut first = true;

    write_ndjson(
        out,
        &serde_json::json!({
            "schema": SCHEMA,
            "kind": "header",
            "root_pid": root_pid,
            "binary": args.binary,
            "binary_sha256": binary_hash,
            "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "warmup_ms": args.warmup.as_millis(),
            "duration_ms": args.duration.as_millis(),
            "interval_ms": args.interval.as_millis(),
            "require_silent_mpv": args.require_silent_mpv,
        }),
    )?;

    while first || started.elapsed() < deadline {
        first = false;
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("query ytt child status: {e}"))?
        {
            return Err(format!(
                "ytt exited unexpectedly before sampling completed: {status}"
            ));
        }

        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let root_sys = sysinfo::Pid::from_u32(root_pid);
        let root = system
            .process(root_sys)
            .ok_or_else(|| format!("root PID {root_pid} disappeared"))?;
        let root_start = root.start_time();
        match identities.insert(root_pid, root_start) {
            Some(previous) if previous != root_start => {
                return Err(format!("root PID {root_pid} was reused"));
            }
            _ => {}
        }

        let tree = descendant_pids(&system, root_pid);
        let observed_at = Instant::now();
        let mut process_samples = Vec::with_capacity(tree.len());
        let mut roles: BTreeMap<&'static str, RoleSample> = BTreeMap::new();
        let mut observed_mpv_identities = Vec::new();
        let mut mpv_silence = Vec::new();

        for pid in tree {
            let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
                continue;
            };
            let start_time = process.start_time();
            if let Some(previous) = identities.insert(pid, start_time)
                && previous != start_time
            {
                return Err(format!(
                    "PID reuse detected for {pid}: start time changed from {previous} to {start_time}"
                ));
            }

            let accumulated_ms = process.accumulated_cpu_time();
            let cpu_percent = cpu_points
                .insert(
                    pid,
                    CpuPoint {
                        start_time,
                        accumulated_ms,
                        observed_at,
                    },
                )
                .filter(|previous| previous.start_time == start_time)
                .and_then(|previous| {
                    let wall_ms = observed_at
                        .duration_since(previous.observed_at)
                        .as_secs_f64()
                        * 1_000.0;
                    (wall_ms > 0.0).then(|| {
                        accumulated_ms.saturating_sub(previous.accumulated_ms) as f64 / wall_ms
                            * 100.0
                    })
                })
                .unwrap_or(0.0);
            let command = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let role = process_role(root_pid, pid, &process.name().to_string_lossy(), &command);
            if role == "mpv" {
                mpv_silence.push(silent_mpv_args(&command));
                if let Some(input_ipc_server_argv) = input_ipc_server_argv(&command) {
                    observed_mpv_identities.push(MpvIdentity {
                        pid,
                        start_time_unix_s: start_time,
                        input_ipc_server_argv,
                    });
                }
            }
            let sample = roles.entry(role).or_default();
            sample.processes += 1;
            sample.cpu_percent += cpu_percent;
            sample.rss_bytes = sample.rss_bytes.saturating_add(process.memory());
            process_samples.push(ProcessSample {
                pid,
                parent_pid: process.parent().map(sysinfo::Pid::as_u32),
                role,
                name: process.name().to_string_lossy().into_owned(),
                start_time_unix_s: start_time,
                accumulated_cpu_ms: accumulated_ms,
                cpu_percent,
                rss_bytes: process.memory(),
                command,
            });
        }

        let total = roles
            .values()
            .copied()
            .fold(RoleSample::default(), |mut sum, item| {
                sum.processes += item.processes;
                sum.cpu_percent += item.cpu_percent;
                sum.rss_bytes = sum.rss_bytes.saturating_add(item.rss_bytes);
                sum
            });
        roles.insert("tree", total);
        let phase = if started.elapsed() < args.warmup {
            "warmup"
        } else {
            measured_mpv_proof.observe(&mpv_silence, observed_mpv_identities.len());
            for (role, sample) in &roles {
                measured.entry(role).or_default().push(*sample);
            }
            "measure"
        };
        if !observed_mpv_identities.is_empty() {
            *last_mpv_identities = observed_mpv_identities;
        }
        write_ndjson(
            out,
            &serde_json::json!({
                "schema": SCHEMA,
                "kind": "sample",
                "elapsed_ms": started.elapsed().as_millis(),
                "phase": phase,
                "mpv_present": !mpv_silence.is_empty(),
                "mpv_all_silent_this_sample": !mpv_silence.is_empty()
                    && mpv_silence.iter().all(|silent| *silent),
                "roles": roles,
                "processes": process_samples,
            }),
        )?;
        out.flush().map_err(|e| format!("flush samples: {e}"))?;

        let next_index =
            (started.elapsed().as_secs_f64() / args.interval.as_secs_f64()).floor() as u32 + 1;
        let next = args.interval.saturating_mul(next_index);
        if next > started.elapsed() {
            thread::sleep(next - started.elapsed());
        }
    }

    let silent_mpv_proven = measured_mpv_proof.proven();
    if args.require_silent_mpv && !silent_mpv_proven {
        return Err(format!(
            "measured phase requires mpv in every sample with effective last-option-wins \
             --ao=null and --volume=0 (samples={}, with_mpv={}, all_silent={})",
            measured_mpv_proof.samples,
            measured_mpv_proof.samples_with_mpv,
            measured_mpv_proof.samples_all_silent,
        ));
    }
    if args.require_silent_mpv && !measured_mpv_proof.cleanup_identities_proven() {
        return Err(format!(
            "every measured mpv must expose an exact --input-ipc-server argv identity for \
             cleanup (samples={}, all_identified={})",
            measured_mpv_proof.samples, measured_mpv_proof.samples_all_cleanup_identified,
        ));
    }
    let summary = measured
        .iter()
        .map(|(role, aggregate)| ((*role).to_string(), aggregate.json()))
        .collect::<serde_json::Map<_, _>>();
    write_ndjson(
        out,
        &serde_json::json!({
            "schema": SCHEMA,
            "kind": "summary",
            "root_pid": root_pid,
            "silent_mpv_proven": silent_mpv_proven,
            "measured_mpv_proof": measured_mpv_proof,
            "last_observed_mpv": last_mpv_identities,
            "roles": summary,
        }),
    )?;
    Ok(())
}

fn descendant_pids(system: &System, root: u32) -> Vec<u32> {
    let mut included = HashSet::from([root]);
    loop {
        let before = included.len();
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| included.contains(&parent.as_u32()))
            {
                included.insert(pid.as_u32());
            }
        }
        if included.len() == before {
            break;
        }
    }
    let mut pids = included.into_iter().collect::<Vec<_>>();
    pids.sort_unstable();
    pids
}

fn process_role(root_pid: u32, pid: u32, name: &str, command: &[String]) -> &'static str {
    if pid == root_pid {
        return "ytt";
    }
    let name = name.to_ascii_lowercase();
    let argv0 = command
        .first()
        .and_then(|value| Path::new(value).file_stem())
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "mpv" || name == "mpv.exe" || argv0 == "mpv" {
        "mpv"
    } else {
        "other"
    }
}

/// mpv is last-option-wins. Merely finding the two safety flags is insufficient because a
/// later argument could re-enable sound, so this parser records the final value of each option.
fn silent_mpv_args(command: &[String]) -> bool {
    let mut ao = None;
    let mut volume = None;
    let mut index = 0;
    while index < command.len() {
        let arg = &command[index];
        if let Some(value) = arg.strip_prefix("--ao=") {
            ao = Some(value.to_ascii_lowercase());
        } else if arg == "--ao" && index + 1 < command.len() {
            index += 1;
            ao = Some(command[index].to_ascii_lowercase());
        } else if let Some(value) = arg.strip_prefix("--volume=") {
            volume = Some(value.to_string());
        } else if arg == "--volume" && index + 1 < command.len() {
            index += 1;
            volume = Some(command[index].clone());
        }
        index += 1;
    }
    ao.as_deref() == Some("null")
        && volume
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok())
            .is_some_and(|value| value == 0.0)
}

/// Preserve the exact argv spelling for every IPC-server occurrence. Both `--key=value` and
/// split `--key value` forms are supported; a malformed split option is not an identity.
fn input_ipc_server_argv(command: &[String]) -> Option<Vec<String>> {
    let mut identity = Vec::new();
    let mut saw_nonempty_value = false;
    let mut index = 0;
    while index < command.len() {
        let arg = &command[index];
        if let Some(value) = arg.strip_prefix("--input-ipc-server=") {
            saw_nonempty_value |= !value.is_empty();
            identity.push(arg.clone());
        } else if arg == "--input-ipc-server" {
            let value = command.get(index + 1)?;
            saw_nonempty_value |= !value.is_empty();
            identity.push(arg.clone());
            identity.push(value.clone());
            index += 1;
        }
        index += 1;
    }
    saw_nonempty_value.then_some(identity)
}

fn exact_mpv_identity_matches(
    identity: &MpvIdentity,
    pid: u32,
    start_time_unix_s: u64,
    command: &[String],
) -> bool {
    identity.pid == pid
        && identity.start_time_unix_s == start_time_unix_s
        && input_ipc_server_argv(command).as_ref() == Some(&identity.input_ipc_server_argv)
}

fn write_pid_file(path: &Path, pid: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create pid directory {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{pid}\n"))
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("publish {}: {e}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn write_ndjson(out: &mut BufWriter<File>, value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value).map_err(|e| format!("encode NDJSON: {e}"))?;
    out.write_all(b"\n")
        .map_err(|e| format!("write NDJSON: {e}"))
}

fn shutdown_child(
    binary: &Path,
    child: &mut Child,
    last_mpv_identities: &[MpvIdentity],
    require_mpv_identity: bool,
) -> Result<(), String> {
    shutdown_child_with_grace(
        binary,
        child,
        last_mpv_identities,
        require_mpv_identity,
        Duration::from_secs(5),
    )
}

fn shutdown_child_with_grace(
    binary: &Path,
    child: &mut Child,
    last_mpv_identities: &[MpvIdentity],
    require_mpv_identity: bool,
    clean_grace: Duration,
) -> Result<(), String> {
    let mut hard_fallback = false;
    if child
        .try_wait()
        .map_err(|e| format!("query ytt status before shutdown: {e}"))?
        .is_none()
    {
        let _ = Command::new(binary)
            .args(["-r", "quit", "-q"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + clean_grace;
        while Instant::now() < deadline {
            if child
                .try_wait()
                .map_err(|e| format!("wait for clean ytt shutdown: {e}"))?
                .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if child
            .try_wait()
            .map_err(|e| format!("query ytt after clean-shutdown grace: {e}"))?
            .is_none()
        {
            hard_fallback = true;
            // This is the exact Child handle launched above, never a process-name lookup. On
            // Unix it is a hard signal and can bypass ytt Drop hooks, so the recorded exact mpv
            // descendants are verified and reaped below.
            child
                .kill()
                .map_err(|e| format!("hard-kill exact ytt child PID {}: {e}", child.id()))?;
            child
                .wait()
                .map_err(|e| format!("wait for exact ytt child PID {}: {e}", child.id()))?;
        }
    }

    if hard_fallback && require_mpv_identity && last_mpv_identities.is_empty() {
        return Err(
            "hard ytt fallback had no recorded exact mpv PID/start-time/IPC argv identity"
                .to_string(),
        );
    }
    for identity in last_mpv_identities {
        stop_exact_mpv(identity, hard_fallback)?;
    }
    Ok(())
}

fn stop_exact_mpv(identity: &MpvIdentity, hard_fallback: bool) -> Result<(), String> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();

    // Give the normal ytt Drop/Windows Job Object path a short opportunity to finish. After a
    // hard ytt kill we refresh immediately; the descendant may already have been reparented.
    let natural_deadline = Instant::now()
        + if hard_fallback {
            Duration::ZERO
        } else {
            Duration::from_secs(2)
        };
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if !system
            .process(sysinfo::Pid::from_u32(identity.pid))
            .is_some_and(|process| {
                let command = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                exact_mpv_identity_matches(identity, identity.pid, process.start_time(), &command)
            })
        {
            return Ok(());
        }
        if Instant::now() >= natural_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let pid = sysinfo::Pid::from_u32(identity.pid);
    let process = system
        .process(pid)
        .ok_or_else(|| format!("exact mpv PID {} disappeared during cleanup", identity.pid))?;
    let term_sent = process.kill_with(Signal::Term).unwrap_or(false);
    if !term_sent && !process.kill() {
        return Err(format!(
            "failed to terminate exact mpv PID {} after identity verification",
            identity.pid
        ));
    }

    let term_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < term_deadline {
        thread::sleep(Duration::from_millis(50));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let still_exact = system.process(pid).is_some_and(|process| {
            let command = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            exact_mpv_identity_matches(identity, identity.pid, process.start_time(), &command)
        });
        if !still_exact {
            return Ok(());
        }
    }

    // TERM can be ignored on Unix and is unavailable on some Windows builds. Re-verify all
    // identity components immediately before the final hard signal.
    let Some(process) = system.process(pid) else {
        return Ok(());
    };
    let command = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if !exact_mpv_identity_matches(identity, identity.pid, process.start_time(), &command) {
        return Ok(());
    }
    if !process.kill() {
        return Err(format!(
            "failed to hard-kill exact mpv PID {} after TERM grace",
            identity.pid
        ));
    }

    let kill_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < kill_deadline {
        thread::sleep(Duration::from_millis(50));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let still_exact = system.process(pid).is_some_and(|process| {
            let command = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            exact_mpv_identity_matches(identity, identity.pid, process.start_time(), &command)
        });
        if !still_exact {
            return Ok(());
        }
    }
    Err(format!(
        "exact mpv PID {} remained after verified cleanup and wait",
        identity.pid
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        MeasuredMpvProof, MpvIdentity, exact_mpv_identity_matches, input_ipc_server_argv,
        shutdown_child_with_grace, silent_mpv_args, stop_exact_mpv,
    };
    use yututui::api::{Song, validate_playable_url, validate_playback_target_for_handoff};
    use yututui::search_source::SearchSource;

    #[test]
    fn silence_check_honors_last_option_wins() {
        let safe = vec!["mpv", "--ao=null", "--volume=0"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(silent_mpv_args(&safe));

        let unsafe_ao = vec!["mpv", "--ao=null", "--volume=0", "--ao=coreaudio"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(!silent_mpv_args(&unsafe_ao));

        let split = vec!["mpv", "--ao", "null", "--volume", "0.0"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(silent_mpv_args(&split));
    }

    #[test]
    fn silent_mpv_proof_requires_every_measured_sample_not_warmup_evidence() {
        let mut warmup_only = MeasuredMpvProof::default();
        assert!(
            !warmup_only.proven(),
            "warmup observations are never recorded here"
        );

        warmup_only.observe(&[], 0);
        warmup_only.observe(&[true], 1);
        assert!(
            !warmup_only.proven(),
            "a missing measured mpv sample must invalidate the proof"
        );

        let mut unsafe_later = MeasuredMpvProof::default();
        unsafe_later.observe(&[true], 1);
        unsafe_later.observe(&[false], 1);
        assert!(
            !unsafe_later.proven(),
            "a later last-option-wins sound override must invalidate the proof"
        );

        let mut measured_safe = MeasuredMpvProof::default();
        measured_safe.observe(&[true], 1);
        measured_safe.observe(&[true], 1);
        assert!(measured_safe.proven());
        assert!(measured_safe.cleanup_identities_proven());

        let mut missing_identity = MeasuredMpvProof::default();
        missing_identity.observe(&[true], 0);
        assert!(missing_identity.proven());
        assert!(!missing_identity.cleanup_identities_proven());
    }

    #[test]
    fn hard_fallback_cleanup_requires_pid_start_time_and_exact_ipc_argv() {
        let equals = vec![
            "mpv".to_string(),
            "--input-ipc-server=/tmp/ytt-run-7/mpv.sock".to_string(),
            "--ao=null".to_string(),
        ];
        let identity = MpvIdentity {
            pid: 4_242,
            start_time_unix_s: 99,
            input_ipc_server_argv: input_ipc_server_argv(&equals).expect("extract IPC argv"),
        };
        assert!(exact_mpv_identity_matches(&identity, 4_242, 99, &equals));
        assert!(!exact_mpv_identity_matches(&identity, 4_243, 99, &equals));
        assert!(!exact_mpv_identity_matches(&identity, 4_242, 100, &equals));

        let reused_endpoint = vec![
            "mpv".to_string(),
            "--input-ipc-server=/tmp/another-run/mpv.sock".to_string(),
            "--ao=null".to_string(),
        ];
        assert!(!exact_mpv_identity_matches(
            &identity,
            4_242,
            99,
            &reused_endpoint,
        ));

        let split = vec![
            "mpv".to_string(),
            "--input-ipc-server".to_string(),
            r"C:\ytt-perf\run\mpv.sock".to_string(),
        ];
        assert_eq!(
            input_ipc_server_argv(&split),
            Some(vec![
                "--input-ipc-server".to_string(),
                r"C:\ytt-perf\run\mpv.sock".to_string(),
            ])
        );
    }

    #[test]
    fn verified_hard_fallback_terminates_and_waits_for_the_exact_process() {
        let ipc_arg = format!(
            "--input-ipc-server={}/ytt-perf-cleanup-{}-{}.sock",
            std::env::temp_dir().display(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        );
        let mut child = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args([
                "--ignored",
                "--exact",
                "tests::cleanup_wait_helper_process",
                "--",
                &ipc_arg,
            ])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn cleanup helper");
        let pid = child.id();
        let refresh = sysinfo::ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::Always)
            .without_tasks();
        let mut system = sysinfo::System::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let identity = loop {
            system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);
            if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
                let command = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if let Some(input_ipc_server_argv) = input_ipc_server_argv(&command) {
                    break MpvIdentity {
                        pid,
                        start_time_unix_s: process.start_time(),
                        input_ipc_server_argv,
                    };
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cleanup helper never published its argv"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        // Reap the direct test child concurrently. In production mpv is ytt's descendant and
        // is reaped by the OS after ytt is gone; this preserves the same observable lifecycle.
        let waiter = std::thread::spawn(move || child.wait());
        stop_exact_mpv(&identity, true).expect("verified cleanup succeeds");
        let status = waiter
            .join()
            .expect("waiter thread")
            .expect("wait cleanup helper");
        assert!(!status.success(), "helper must have been terminated");
    }

    #[test]
    fn child_hard_fallback_also_cleans_the_recorded_exact_mpv() {
        let current_exe = std::env::current_exe().expect("test exe");
        let helper_args = [
            "--ignored",
            "--exact",
            "tests::cleanup_wait_helper_process",
            "--",
        ];
        let mut ytt = std::process::Command::new(&current_exe)
            .args(helper_args)
            .arg("ytt-owner-helper")
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ytt owner helper");
        let ipc_arg = format!(
            "--input-ipc-server={}/ytt-perf-fallback-{}-{}.sock",
            std::env::temp_dir().display(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        );
        let mut mpv = std::process::Command::new(&current_exe)
            .args(helper_args)
            .arg(&ipc_arg)
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn mpv helper");
        let mpv_pid = mpv.id();
        let refresh = sysinfo::ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::Always)
            .without_tasks();
        let mut system = sysinfo::System::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let identity = loop {
            system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);
            if let Some(process) = system.process(sysinfo::Pid::from_u32(mpv_pid)) {
                let command = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if let Some(input_ipc_server_argv) = input_ipc_server_argv(&command) {
                    break MpvIdentity {
                        pid: mpv_pid,
                        start_time_unix_s: process.start_time(),
                        input_ipc_server_argv,
                    };
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mpv helper never published its argv"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mpv_waiter = std::thread::spawn(move || mpv.wait());

        shutdown_child_with_grace(
            &current_exe,
            &mut ytt,
            &[identity],
            true,
            std::time::Duration::from_millis(20),
        )
        .expect("hard fallback cleans exact child and mpv");
        assert!(
            ytt.try_wait().expect("query ytt helper").is_some(),
            "exact ytt child must have been reaped"
        );
        let mpv_status = mpv_waiter
            .join()
            .expect("mpv waiter thread")
            .expect("wait mpv helper");
        assert!(!mpv_status.success(), "exact mpv helper must be terminated");
    }

    #[test]
    #[ignore]
    fn cleanup_wait_helper_process() {
        if std::env::var_os("TUI_PERF_CLEANUP_HELPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_m3u_indirection_passes_both_playback_url_guards() {
        assert!(
            validate_playable_url(
                SearchSource::RadioBrowser,
                "http://127.0.0.1:12345/fixture.wav",
            )
            .is_err(),
            "a seeded direct loopback URL must remain forbidden",
        );

        let playlist = if cfg!(windows) {
            PathBuf::from(r"C:\ytt-perf\home\fixture\tui-perf-stream.m3u")
        } else {
            PathBuf::from("/ytt-perf/home/fixture/tui-perf-stream.m3u")
        };
        let target = Song::local_file(playlist.clone())
            .playback_target_checked()
            .expect("a local playlist path is a valid Song playback target");
        assert_eq!(target, playlist.to_string_lossy());
        assert_eq!(
            validate_playback_target_for_handoff(&target)
                .await
                .expect("player handoff accepts local paths"),
            target,
        );
    }
}
