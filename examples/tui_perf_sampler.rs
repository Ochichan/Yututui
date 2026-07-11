//! Cross-platform process-tree sampler used by `scripts/tui-perf.{sh,ps1}`.
//!
//! The sampler must itself run in an interactive terminal (tmux on Unix, a local
//! ConPTY/console on Windows). It launches exactly one `ytt`, follows only that PID's
//! descendants, and writes NDJSON outside the measured terminal. No process-name-wide
//! cleanup is ever performed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};

const SCHEMA: &str = "ytt.tui-perf.samples.v1";
const CLEANUP_SCOPE: &str = "dedicated_owner_process_group_and_observed_exact_descendants";

#[derive(Debug)]
struct Args {
    output: PathBuf,
    pid_file: Option<PathBuf>,
    identity_file: PathBuf,
    controller_ready_file: Option<PathBuf>,
    binary: PathBuf,
    child_args: Vec<String>,
    warmup: Duration,
    duration: Duration,
    interval: Duration,
    require_silent_mpv: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut output = None;
        let mut pid_file = None;
        let mut identity_file = None;
        let mut controller_ready_file = None;
        let mut binary = None;
        let mut warmup_secs = 0.0;
        let mut duration_secs = 60.0;
        let mut interval_ms = 1_000u64;
        let mut require_silent_mpv = false;
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
                "--identity-file" => {
                    identity_file = Some(PathBuf::from(value("--identity-file", &mut raw)?))
                }
                "--controller-ready-file" => {
                    controller_ready_file =
                        Some(PathBuf::from(value("--controller-ready-file", &mut raw)?))
                }
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
                "-h" | "--help" => return Err(usage().to_string()),
                other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
            }
        }

        Ok(Self {
            output: output.ok_or_else(|| "--output is required".to_string())?,
            pid_file,
            identity_file: identity_file
                .ok_or_else(|| "--identity-file is required".to_string())?,
            controller_ready_file,
            binary: binary.ok_or_else(|| "--binary is required".to_string())?,
            child_args,
            warmup: Duration::from_secs_f64(warmup_secs),
            duration: Duration::from_secs_f64(duration_secs),
            interval: Duration::from_millis(interval_ms),
            require_silent_mpv,
        })
    }
}

fn usage() -> &'static str {
    "Usage: tui_perf_sampler --output FILE --binary YTT [options] [-- YTT_ARGS...]\n\
     Options:\n\
       --pid-file FILE          Write the launched ytt PID atomically\n\
       --identity-file FILE     Atomically maintain exact owner/mpv cleanup identity\n\
       --controller-ready-file FILE\n\
                                Wait for confirmed mpv subscriptions before sampling\n\
       --warmup-secs N          Warm-up samples excluded from the summary (default 0)\n\
       --duration-secs N        Measured duration (default 60)\n\
       --interval-ms N          Sampling interval, at least 100 ms (default 1000)\n\
       --require-silent-mpv     Fail unless effective mpv argv has ao=null and volume=0"
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

#[derive(Clone, Debug, Serialize)]
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
    executable: Option<PathBuf>,
    executable_bytes: Option<u64>,
    executable_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct DescendantIdentity {
    pid: u32,
    start_time_unix_s: u64,
    executable: PathBuf,
    executable_bytes: u64,
    executable_sha256: String,
    role: &'static str,
    command: Vec<String>,
}

/// The exact mpv descendant identity retained for cleanup after ytt exits. `start_time_unix_s`
/// protects against ordinary PID reuse, while the run-unique IPC argv protects platforms whose
/// process start time is only reported at second resolution. Cleanup never falls back to a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MpvIdentity {
    pid: u32,
    start_time_unix_s: u64,
    executable: PathBuf,
    executable_bytes: u64,
    executable_sha256: String,
    input_ipc_server_argv: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LiveProcessIdentity {
    pid: u32,
    start_time_unix_s: u64,
    process_group_id: Option<u32>,
    executable: PathBuf,
    executable_bytes: u64,
    executable_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PartialProcessIdentity {
    pid: u32,
    start_time_unix_s: u64,
    process_group_id: Option<u32>,
}

type ProcessInventory = (Vec<DescendantIdentity>, Vec<MpvIdentity>);

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
    cpu_weighted_percent_seconds: f64,
    rss_sum: u128,
    rss_peak: u64,
}

impl Aggregate {
    fn push(&mut self, value: RoleSample, cpu_interval_overlap: Duration) {
        self.samples += 1;
        self.cpu_weighted_percent_seconds += value.cpu_percent * cpu_interval_overlap.as_secs_f64();
        self.rss_sum += u128::from(value.rss_bytes);
        self.rss_peak = self.rss_peak.max(value.rss_bytes);
    }

    fn json(&self, cpu_window: Duration) -> serde_json::Value {
        let rss_divisor = self.samples.max(1);
        serde_json::json!({
            "samples": self.samples,
            "mean_cpu_percent": self.cpu_weighted_percent_seconds / cpu_window.as_secs_f64(),
            "mean_rss_bytes": (self.rss_sum / u128::from(rss_divisor)) as u64,
            "peak_rss_bytes": self.rss_peak,
        })
    }
}

fn cpu_interval_overlap(
    previous: Option<Duration>,
    observed: Duration,
    window_start: Duration,
    window_end: Duration,
) -> Duration {
    let Some(previous) = previous else {
        return Duration::ZERO;
    };
    let overlap_start = previous.max(window_start);
    let overlap_end = observed.min(window_end);
    overlap_end.saturating_sub(overlap_start)
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
    let run_id =
        std::env::var("TUI_PERF_RUN_ID").map_err(|_| "TUI_PERF_RUN_ID is required".to_string())?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
    }
    let file =
        File::create(&args.output).map_err(|e| format!("create {}: {e}", args.output.display()))?;
    let mut out = BufWriter::new(file);

    let binary_hash = sha256_file(&args.binary)?;
    let producer_binary_hash = producer_binary_sha256()?;
    let producer_identity = current_process_identity()?;
    let mut last_mpv_identities = Vec::new();
    let mut last_descendant_identities = Vec::new();
    write_live_identity(
        &args.identity_file,
        "startup",
        &producer_identity,
        None,
        None,
        &last_mpv_identities,
        &last_descendant_identities,
        false,
    )?;
    let mut owner_command = Command::new(&args.binary);
    owner_command
        .args(&args.child_args)
        .env_remove("YTM_PERF")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    owner_command.process_group(0);
    let mut child = owner_command
        .spawn()
        .map_err(|e| format!("launch {}: {e}", args.binary.display()))?;
    let root_pid = child.id();
    let partial_owner = match wait_for_partial_process_identity(root_pid, &mut child) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(cleanup_after_startup_error(
                &args,
                &producer_identity,
                None,
                &mut child,
                error,
            ));
        }
    };
    if let Err(error) = write_live_identity(
        &args.identity_file,
        "owner_starting",
        &producer_identity,
        None,
        Some(&partial_owner),
        &last_mpv_identities,
        &last_descendant_identities,
        false,
    ) {
        return Err(cleanup_after_startup_error(
            &args,
            &producer_identity,
            Some(&partial_owner),
            &mut child,
            error,
        ));
    }
    let owner_identity = match wait_for_process_identity(root_pid, &binary_hash, &mut child) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(cleanup_after_startup_error(
                &args,
                &producer_identity,
                Some(&partial_owner),
                &mut child,
                error,
            ));
        }
    };
    if let Err(error) = write_live_identity(
        &args.identity_file,
        "running",
        &producer_identity,
        Some(&owner_identity),
        Some(&partial_owner),
        &last_mpv_identities,
        &last_descendant_identities,
        false,
    ) {
        return Err(cleanup_after_startup_error(
            &args,
            &producer_identity,
            Some(&partial_owner),
            &mut child,
            error,
        ));
    }

    if let Some(path) = &args.pid_file
        && let Err(error) = write_pid_file(path, root_pid)
    {
        return Err(cleanup_after_startup_error(
            &args,
            &producer_identity,
            Some(&partial_owner),
            &mut child,
            error,
        ));
    }
    let barrier_result = if let Some(path) = &args.controller_ready_file {
        wait_for_controller_ready(
            path,
            root_pid,
            &mut child,
            &args.identity_file,
            &producer_identity,
            &owner_identity,
            &run_id,
            &mut last_mpv_identities,
            &mut last_descendant_identities,
        )
    } else {
        // Close the smaller non-controller startup window before the first timed sample.
        capture_live_descendant_identities(
            root_pid,
            &args.identity_file,
            &producer_identity,
            &owner_identity,
            &mut last_mpv_identities,
            &mut last_descendant_identities,
        )
    };
    let sample_result = barrier_result.and_then(|()| {
        sample_process_tree(
            &args,
            root_pid,
            binary_hash,
            producer_binary_hash,
            &run_id,
            &mut child,
            &mut out,
            &mut last_mpv_identities,
            &mut last_descendant_identities,
            &producer_identity,
            &owner_identity,
        )
    });
    let cleanup_result = shutdown_measured_tree(
        &args.binary,
        &mut child,
        &producer_identity,
        &owner_identity,
        &mut last_mpv_identities,
        &mut last_descendant_identities,
        args.require_silent_mpv,
        |mpv, descendants| {
            write_live_identity(
                &args.identity_file,
                "cleanup_requested",
                &producer_identity,
                Some(&owner_identity),
                Some(&partial_owner),
                mpv,
                descendants,
                false,
            )
        },
    );
    let cleanup_succeeded = cleanup_result.is_ok();
    let mut result = match (sample_result, cleanup_result) {
        (Err(sample), Err(cleanup)) => Err(format!("{sample}; cleanup also failed: {cleanup}")),
        (Err(sample), Ok(())) => Err(sample),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    };
    if cleanup_succeeded
        && let Err(cleanup_proof_error) = write_live_identity(
            &args.identity_file,
            "cleaned",
            &producer_identity,
            Some(&owner_identity),
            Some(&partial_owner),
            &last_mpv_identities,
            &last_descendant_identities,
            true,
        )
    {
        result = Err(match result {
            Ok(()) => cleanup_proof_error,
            Err(error) => format!(
                "{error}; publishing completed cleanup proof also failed: {cleanup_proof_error}"
            ),
        });
    }
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

#[allow(clippy::too_many_arguments)]
fn sample_process_tree(
    args: &Args,
    root_pid: u32,
    binary_hash: String,
    producer_binary_hash: String,
    run_id: &str,
    child: &mut Child,
    out: &mut BufWriter<File>,
    last_mpv_identities: &mut Vec<MpvIdentity>,
    last_descendant_identities: &mut Vec<DescendantIdentity>,
    producer_identity: &LiveProcessIdentity,
    owner_identity: &LiveProcessIdentity,
) -> Result<(), String> {
    let started = Instant::now();
    let terminal_geometry = crossterm::terminal::size()
        .map_err(|error| format!("query measured PTY geometry: {error}"))?;
    let observation_started_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before Unix epoch: {e}"))?
        .as_nanos();
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
    let mut executable_identities: HashMap<(u32, u64), (PathBuf, u64, String)> = HashMap::new();
    let mut measured: BTreeMap<&'static str, Aggregate> = BTreeMap::new();
    let mut measured_mpv_proof = MeasuredMpvProof::default();
    let mut previous_observed_elapsed = None;

    write_ndjson(
        out,
        &header_record(
            args,
            root_pid,
            &binary_hash,
            &producer_binary_hash,
            run_id,
            observation_started_unix_ns,
            terminal_geometry,
        ),
    )?;

    loop {
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
        let observed_elapsed = observed_at.saturating_duration_since(started);
        let cpu_overlap = cpu_interval_overlap(
            previous_observed_elapsed,
            observed_elapsed,
            args.warmup,
            deadline,
        );
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
            let executable_identity =
                if let Some(identity) = executable_identities.get(&(pid, start_time)) {
                    Some(identity.clone())
                } else if let Some(executable) = process.exe() {
                    let canonical = executable.canonicalize().map_err(|e| {
                        format!(
                            "canonicalize executable for PID {pid} {}: {e}",
                            executable.display()
                        )
                    })?;
                    let bytes = canonical
                        .metadata()
                        .map_err(|e| {
                            format!("stat executable for PID {pid} {}: {e}", canonical.display())
                        })?
                        .len();
                    let sha256 = sha256_file(&canonical)?;
                    let identity = (canonical, bytes, sha256);
                    executable_identities.insert((pid, start_time), identity.clone());
                    Some(identity)
                } else {
                    None
                };
            if executable_identity.is_none() {
                return Err(format!(
                    "measured {role} PID {pid} has no executable identity; exact recursive cleanup cannot be proven"
                ));
            }
            if pid == root_pid
                && executable_identity
                    .as_ref()
                    .is_some_and(|(_, _, sha256)| sha256 != &binary_hash)
            {
                return Err(
                    "root executable hash differs from the requested ytt binary".to_string()
                );
            }
            if role == "mpv" {
                mpv_silence.push(silent_mpv_args(&command));
                if let Some(input_ipc_server_argv) = input_ipc_server_argv(&command) {
                    let (executable, executable_bytes, executable_sha256) = executable_identity
                        .as_ref()
                        .expect("mpv executable identity is required");
                    observed_mpv_identities.push(MpvIdentity {
                        pid,
                        start_time_unix_s: start_time,
                        executable: executable.clone(),
                        executable_bytes: *executable_bytes,
                        executable_sha256: executable_sha256.clone(),
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
                executable: executable_identity
                    .as_ref()
                    .map(|(path, _, _)| path.clone()),
                executable_bytes: executable_identity.as_ref().map(|(_, bytes, _)| *bytes),
                executable_sha256: executable_identity.map(|(_, _, sha256)| sha256),
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
        let phase = if observed_elapsed < args.warmup {
            "warmup"
        } else {
            measured_mpv_proof.observe(&mpv_silence, observed_mpv_identities.len());
            for (role, sample) in &roles {
                measured.entry(role).or_default().push(*sample, cpu_overlap);
            }
            "measure"
        };
        let observed_descendant_identities = process_samples
            .iter()
            .filter(|process| process.pid != root_pid)
            .map(|process| DescendantIdentity {
                pid: process.pid,
                start_time_unix_s: process.start_time_unix_s,
                executable: process
                    .executable
                    .clone()
                    .expect("every measured descendant has an executable identity"),
                executable_bytes: process
                    .executable_bytes
                    .expect("every measured descendant has executable bytes"),
                executable_sha256: process
                    .executable_sha256
                    .clone()
                    .expect("every measured descendant has an executable hash"),
                role: process.role,
                command: process.command.clone(),
            })
            .collect::<Vec<_>>();
        let observed_descendant_identities = merge_live_descendants(
            &system,
            last_descendant_identities,
            observed_descendant_identities,
        );
        let observed_mpv_identities =
            merge_live_mpv(&system, last_mpv_identities, observed_mpv_identities);
        let identity_changed = *last_descendant_identities != observed_descendant_identities
            || *last_mpv_identities != observed_mpv_identities;
        *last_descendant_identities = observed_descendant_identities;
        *last_mpv_identities = observed_mpv_identities;
        if identity_changed {
            write_live_identity(
                &args.identity_file,
                "running",
                producer_identity,
                Some(owner_identity),
                None,
                last_mpv_identities,
                last_descendant_identities,
                false,
            )?;
        }
        write_ndjson(
            out,
            &serde_json::json!({
                "schema": SCHEMA,
                "kind": "sample",
                "elapsed_ms": observed_elapsed.as_millis(),
                "observed_elapsed_ns": observed_elapsed.as_nanos(),
                "cpu_interval_overlap_ns": cpu_overlap.as_nanos(),
                "phase": phase,
                "mpv_present": !mpv_silence.is_empty(),
                "mpv_all_silent_this_sample": !mpv_silence.is_empty()
                    && mpv_silence.iter().all(|silent| *silent),
                "roles": roles,
                "processes": process_samples,
            }),
        )?;
        out.flush().map_err(|e| format!("flush samples: {e}"))?;
        previous_observed_elapsed = Some(observed_elapsed);

        if observed_elapsed >= deadline {
            break;
        }

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
        .map(|(role, aggregate)| ((*role).to_string(), aggregate.json(args.duration)))
        .collect::<serde_json::Map<_, _>>();
    write_ndjson(
        out,
        &serde_json::json!({
            "schema": SCHEMA,
            "kind": "summary",
            "run_id": run_id,
            "cpu_accounting": "time_weighted_counter_deltas_clamped_to_measure_window",
            "cpu_window_start_ns": args.warmup.as_nanos(),
            "cpu_window_end_ns": deadline.as_nanos(),
            "terminal_geometry": [terminal_geometry.0, terminal_geometry.1],
            "root_pid": root_pid,
            "silent_mpv_proven": silent_mpv_proven,
            "measured_mpv_proof": measured_mpv_proof,
            "last_observed_mpv": last_mpv_identities,
            "observation_finished_unix_ns": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("system clock before Unix epoch: {e}"))?
                .as_nanos(),
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

fn mpv_executable_matches(identity: &MpvIdentity, process: &sysinfo::Process) -> bool {
    process
        .exe()
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|path| path == identity.executable)
        && identity
            .executable
            .metadata()
            .ok()
            .is_some_and(|metadata| metadata.len() == identity.executable_bytes)
        && sha256_file(&identity.executable).ok().as_deref()
            == Some(identity.executable_sha256.as_str())
}

fn exact_descendant_matches(identity: &DescendantIdentity, process: &sysinfo::Process) -> bool {
    if process.start_time() != identity.start_time_unix_s {
        return false;
    }
    let command = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    command == identity.command
        && process
            .exe()
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == identity.executable)
        && identity
            .executable
            .metadata()
            .ok()
            .is_some_and(|metadata| metadata.len() == identity.executable_bytes)
        && sha256_file(&identity.executable).ok().as_deref()
            == Some(identity.executable_sha256.as_str())
}

fn merge_live_descendants(
    system: &System,
    previous: &[DescendantIdentity],
    current: Vec<DescendantIdentity>,
) -> Vec<DescendantIdentity> {
    let mut merged = current
        .into_iter()
        .map(|identity| ((identity.pid, identity.start_time_unix_s), identity))
        .collect::<BTreeMap<_, _>>();
    for identity in previous {
        let key = (identity.pid, identity.start_time_unix_s);
        if !merged.contains_key(&key)
            && system
                .process(sysinfo::Pid::from_u32(identity.pid))
                .is_some_and(|process| exact_descendant_matches(identity, process))
        {
            merged.insert(key, identity.clone());
        }
    }
    merged.into_values().collect()
}

fn merge_live_mpv(
    system: &System,
    previous: &[MpvIdentity],
    current: Vec<MpvIdentity>,
) -> Vec<MpvIdentity> {
    let mut merged = current
        .into_iter()
        .map(|identity| ((identity.pid, identity.start_time_unix_s), identity))
        .collect::<BTreeMap<_, _>>();
    for identity in previous {
        let key = (identity.pid, identity.start_time_unix_s);
        if !merged.contains_key(&key)
            && system
                .process(sysinfo::Pid::from_u32(identity.pid))
                .is_some_and(|process| {
                    let command = process
                        .cmd()
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned())
                        .collect::<Vec<_>>();
                    mpv_executable_matches(identity, process)
                        && exact_mpv_identity_matches(
                            identity,
                            identity.pid,
                            process.start_time(),
                            &command,
                        )
                })
        {
            merged.insert(key, identity.clone());
        }
    }
    merged.into_values().collect()
}

fn wait_for_partial_process_identity(
    pid: u32,
    child: &mut Child,
) -> Result<PartialProcessIdentity, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let refresh = ProcessRefreshKind::nothing().without_tasks();
    let mut system = System::new();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
            return Ok(PartialProcessIdentity {
                pid,
                start_time_unix_s: process.start_time(),
                process_group_id: process_group_id(pid)?,
            });
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("query owner while capturing startup identity: {e}"))?
        {
            return Err(format!(
                "ytt exited before startup identity capture: {status}"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out capturing startup identity for owner PID {pid}"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_process_identity(
    pid: u32,
    expected_sha256: &str,
    child: &mut Child,
) -> Result<LiveProcessIdentity, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid))
            && let Some(executable) = process.exe()
        {
            let executable = executable
                .canonicalize()
                .map_err(|e| format!("canonicalize owner executable: {e}"))?;
            let executable_bytes = executable
                .metadata()
                .map_err(|e| format!("stat owner executable: {e}"))?
                .len();
            let executable_sha256 = sha256_file(&executable)?;
            if executable_sha256 != expected_sha256 {
                return Err("spawned owner executable hash differs from --binary".to_string());
            }
            return Ok(LiveProcessIdentity {
                pid,
                start_time_unix_s: process.start_time(),
                process_group_id: process_group_id(pid)?,
                executable,
                executable_bytes,
                executable_sha256,
            });
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("query owner while capturing identity: {e}"))?
        {
            return Err(format!("ytt exited before identity capture: {status}"));
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out capturing identity for owner PID {pid}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn current_process_identity() -> Result<LiveProcessIdentity, String> {
    let pid = std::process::id();
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let process = system
        .process(sysinfo::Pid::from_u32(pid))
        .ok_or_else(|| format!("sampler PID {pid} is not observable"))?;
    let executable = process
        .exe()
        .ok_or_else(|| format!("sampler PID {pid} has no executable path"))?
        .canonicalize()
        .map_err(|e| format!("canonicalize sampler executable: {e}"))?;
    let executable_bytes = executable
        .metadata()
        .map_err(|e| format!("stat sampler executable: {e}"))?
        .len();
    Ok(LiveProcessIdentity {
        pid,
        start_time_unix_s: process.start_time(),
        process_group_id: process_group_id(pid)?,
        executable_sha256: sha256_file(&executable)?,
        executable,
        executable_bytes,
    })
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> Result<Option<u32>, String> {
    observed_process_group_id(pid)
        .map(Some)
        .ok_or_else(|| format!("getpgid({pid}) failed: {}", std::io::Error::last_os_error()))
}

#[cfg(not(unix))]
fn process_group_id(_pid: u32) -> Result<Option<u32>, String> {
    Ok(None)
}

#[cfg(unix)]
fn observed_process_group_id(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: getpgid only reads kernel process metadata for the supplied numeric PID.
    let group = unsafe { libc::getpgid(pid) };
    (group >= 0).then_some(group as u32)
}

#[cfg(not(unix))]
fn observed_process_group_id(_pid: u32) -> Option<u32> {
    None
}

fn discover_descendant_identities(
    system: &mut System,
    executable_identities: &mut HashMap<(u32, u64), (PathBuf, u64, String)>,
    root_pid: u32,
    include_process_group: Option<u32>,
    protected_pid: Option<u32>,
) -> Result<Option<ProcessInventory>, String> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .without_tasks();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    if system.process(sysinfo::Pid::from_u32(root_pid)).is_none() {
        return Ok(None);
    }
    let mut candidate_pids = descendant_pids(system, root_pid)
        .into_iter()
        .filter(|pid| *pid != root_pid)
        .collect::<HashSet<_>>();
    if let Some(group) = include_process_group {
        candidate_pids.extend(system.processes().keys().filter_map(|pid| {
            let pid = pid.as_u32();
            (pid != root_pid
                && Some(pid) != protected_pid
                && observed_process_group_id(pid) == Some(group))
            .then_some(pid)
        }));
    }
    let mut candidate_pids = candidate_pids.into_iter().collect::<Vec<_>>();
    candidate_pids.sort_unstable();
    let mut descendants = Vec::new();
    let mut mpv = Vec::new();
    for pid in candidate_pids {
        let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
            return Ok(None);
        };
        let start_time = process.start_time();
        let command = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let role = process_role(root_pid, pid, &process.name().to_string_lossy(), &command);
        let executable_identity =
            if let Some(identity) = executable_identities.get(&(pid, start_time)) {
                identity.clone()
            } else {
                let Some(path) = process.exe() else {
                    // A just-forked process may not have completed exec. Retrying is safer than
                    // publishing an incomplete recursive-tree claim.
                    return Ok(None);
                };
                let executable = path.canonicalize().map_err(|e| {
                    format!(
                        "canonicalize startup descendant executable for PID {pid} {}: {e}",
                        path.display()
                    )
                })?;
                let executable_bytes = executable
                    .metadata()
                    .map_err(|e| {
                        format!(
                            "stat startup descendant executable for PID {pid} {}: {e}",
                            executable.display()
                        )
                    })?
                    .len();
                let identity = (
                    executable.clone(),
                    executable_bytes,
                    sha256_file(&executable)?,
                );
                executable_identities.insert((pid, start_time), identity.clone());
                identity
            };
        let identity = DescendantIdentity {
            pid,
            start_time_unix_s: start_time,
            executable: executable_identity.0.clone(),
            executable_bytes: executable_identity.1,
            executable_sha256: executable_identity.2.clone(),
            role,
            command: command.clone(),
        };
        if role == "mpv"
            && let Some(input_ipc_server_argv) = input_ipc_server_argv(&command)
        {
            mpv.push(MpvIdentity {
                pid,
                start_time_unix_s: start_time,
                executable: executable_identity.0,
                executable_bytes: executable_identity.1,
                executable_sha256: executable_identity.2,
                input_ipc_server_argv,
            });
        }
        descendants.push(identity);
    }
    descendants.sort_by_key(|identity| identity.pid);
    mpv.sort_by_key(|identity| identity.pid);
    Ok(Some((descendants, mpv)))
}

#[allow(clippy::too_many_arguments)]
fn capture_live_descendant_identities_with_system(
    root_pid: u32,
    identity_file: &Path,
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    system: &mut System,
    executable_identities: &mut HashMap<(u32, u64), (PathBuf, u64, String)>,
) -> Result<(), String> {
    let Some((descendants, mpv)) =
        discover_descendant_identities(system, executable_identities, root_pid, None, None)?
    else {
        return Ok(());
    };
    let descendants = merge_live_descendants(system, last_descendants, descendants);
    let mpv = merge_live_mpv(system, last_mpv, mpv);
    if descendants != *last_descendants || mpv != *last_mpv {
        *last_descendants = descendants;
        *last_mpv = mpv;
        write_live_identity(
            identity_file,
            "running",
            producer,
            Some(owner),
            None,
            last_mpv,
            last_descendants,
            false,
        )?;
    }
    Ok(())
}

fn capture_live_descendant_identities(
    root_pid: u32,
    identity_file: &Path,
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
) -> Result<(), String> {
    let mut system = System::new();
    let mut executable_identities = HashMap::new();
    capture_live_descendant_identities_with_system(
        root_pid,
        identity_file,
        producer,
        owner,
        last_mpv,
        last_descendants,
        &mut system,
        &mut executable_identities,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_live_identity(
    path: &Path,
    state: &str,
    producer: &LiveProcessIdentity,
    owner: Option<&LiveProcessIdentity>,
    partial_owner: Option<&PartialProcessIdentity>,
    mpv: &[MpvIdentity],
    descendants: &[DescendantIdentity],
    cleanup_proven: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create identity directory {}: {e}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    let updated_unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock before Unix epoch: {e}"))?
        .as_nanos();
    if std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|document| {
            document
                .get("state")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|existing| live_identity_update_is_stale(state, &existing))
    {
        // The external cleanup orchestrator has frozen this producer and published
        // cleanup_requested. Once resumed, it must never be overwritten by stale startup data.
        return Ok(());
    }
    let run_id = std::env::var("TUI_PERF_RUN_ID")
        .map_err(|_| "TUI_PERF_RUN_ID is required for live identity".to_string())?;
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema": "ytt.tui-perf.live-identity.v1",
        "run_id": run_id,
        "state": state,
        "producer": producer,
        "owner": owner,
        "partial_owner": partial_owner,
        "mpv": mpv,
        "descendants": descendants,
        "cleanup_scope": CLEANUP_SCOPE,
        "cleanup_proven": cleanup_proven,
        "updated_unix_ns": updated_unix_ns,
    }))
    .map_err(|e| format!("encode live identity: {e}"))?;
    std::fs::write(&temporary, bytes)
        .map_err(|e| format!("write live identity {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|e| format!("publish live identity {}: {e}", path.display()))
}

fn live_identity_update_is_stale(requested: &str, existing: &str) -> bool {
    matches!(requested, "owner_starting" | "running")
        && !matches!(existing, "startup" | "owner_starting" | "running")
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

#[allow(clippy::too_many_arguments)]
fn wait_for_controller_ready(
    path: &Path,
    root_pid: u32,
    child: &mut Child,
    identity_file: &Path,
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    run_id: &str,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut system = System::new();
    let mut executable_identities = HashMap::new();
    loop {
        capture_live_descendant_identities_with_system(
            root_pid,
            identity_file,
            producer,
            owner,
            last_mpv,
            last_descendants,
            &mut system,
            &mut executable_identities,
        )?;
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && value.get("schema").and_then(serde_json::Value::as_str)
                == Some("ytt.tui-perf.controller-ready.v1")
            && value.get("owner_pid").and_then(serde_json::Value::as_u64)
                == Some(u64::from(root_pid))
            && value.get("run_id").and_then(serde_json::Value::as_str) == Some(run_id)
            && value
                .get("subscriptions_confirmed")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("query ytt while waiting for controller ready: {e}"))?
        {
            return Err(format!(
                "ytt exited before controller subscription barrier: {status}"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for controller subscription barrier {}",
                path.display()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn producer_binary_sha256() -> Result<String, String> {
    let executable =
        std::env::current_exe().map_err(|e| format!("resolve current sampler executable: {e}"))?;
    sha256_file(&executable)
}

fn header_record(
    args: &Args,
    root_pid: u32,
    binary_hash: &str,
    producer_binary_hash: &str,
    run_id: &str,
    observation_started_unix_ns: u128,
    terminal_geometry: (u16, u16),
) -> serde_json::Value {
    serde_json::json!({
        "schema": SCHEMA,
        "kind": "header",
        "root_pid": root_pid,
        "binary": args.binary,
        "binary_sha256": binary_hash,
        "producer_binary_sha256": producer_binary_hash,
        "run_id": run_id,
        "observation_started_unix_ns": observation_started_unix_ns,
        "terminal_geometry": [terminal_geometry.0, terminal_geometry.1],
        "controller_barrier_required": args.controller_ready_file.is_some(),
        "child_ytm_perf_enabled": false,
        "scenario_sha256": std::env::var("TUI_PERF_SCENARIO_SHA256").ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "warmup_ms": args.warmup.as_millis(),
        "duration_ms": args.duration.as_millis(),
        "cpu_accounting": "time_weighted_counter_deltas_clamped_to_measure_window",
        "cpu_window_start_ns": args.warmup.as_nanos(),
        "cpu_window_end_ns": (args.warmup + args.duration).as_nanos(),
        "interval_ms": args.interval.as_millis(),
        "require_silent_mpv": args.require_silent_mpv,
    })
}

fn write_ndjson(out: &mut BufWriter<File>, value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut *out, value).map_err(|e| format!("encode NDJSON: {e}"))?;
    out.write_all(b"\n")
        .map_err(|e| format!("write NDJSON: {e}"))
}

#[cfg(unix)]
fn live_process_identity_matches(
    identity: &LiveProcessIdentity,
    process: &sysinfo::Process,
) -> bool {
    process.start_time() == identity.start_time_unix_s
        && process
            .exe()
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == identity.executable)
        && identity
            .executable
            .metadata()
            .ok()
            .is_some_and(|metadata| metadata.len() == identity.executable_bytes)
        && sha256_file(&identity.executable).ok().as_deref()
            == Some(identity.executable_sha256.as_str())
}

#[cfg(unix)]
fn exact_owner_process_group(
    owner: &LiveProcessIdentity,
    producer: &LiveProcessIdentity,
) -> Result<u32, String> {
    let group = owner.process_group_id.ok_or_else(|| {
        format!(
            "owner PID {} has no process-group identity on Unix",
            owner.pid
        )
    })?;
    if group != owner.pid {
        return Err(format!(
            "owner PID {} is not leader of its dedicated process group {group}",
            owner.pid
        ));
    }
    if Some(group) == producer.process_group_id {
        return Err("owner process group is not isolated from the sampler".to_string());
    }
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let process = system
        .process(sysinfo::Pid::from_u32(owner.pid))
        .ok_or_else(|| format!("exact owner PID {} is no longer present", owner.pid))?;
    if !live_process_identity_matches(owner, process) {
        return Err(format!(
            "owner PID {} changed exact identity before process-group signal",
            owner.pid
        ));
    }
    if observed_process_group_id(owner.pid) != Some(group) {
        return Err(format!(
            "owner PID {} changed process group before process-group signal",
            owner.pid
        ));
    }
    Ok(group)
}

#[cfg(unix)]
fn signal_dedicated_owner_group(
    group: u32,
    producer: &LiveProcessIdentity,
    requested_signal: i32,
) -> Result<(), String> {
    if Some(group) == producer.process_group_id {
        return Err("owner process group is not isolated from the sampler".to_string());
    }
    let group = i32::try_from(group).map_err(|_| "owner PGID does not fit pid_t".to_string())?;
    // SAFETY: a negative PID addresses the already-verified dedicated owner process group.
    if unsafe { libc::kill(-group, requested_signal) } != 0 {
        return Err(format!(
            "signal dedicated owner process group {group}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_dedicated_group_stop(group: u32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut system = System::new();
    while Instant::now() < deadline {
        system.refresh_processes(ProcessesToUpdate::All, true);
        let members = system
            .processes()
            .iter()
            .filter(|(pid, _process)| observed_process_group_id(pid.as_u32()) == Some(group))
            .map(|(_pid, process)| process)
            .collect::<Vec<_>>();
        if !members.is_empty()
            && members
                .iter()
                .all(|process| process.status() == sysinfo::ProcessStatus::Stop)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "dedicated owner process group {group} did not become fully stopped"
    ))
}

#[cfg(unix)]
fn freeze_final_process_inventory<S>(
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    signal_group: &mut S,
) -> Result<(), String>
where
    S: FnMut(u32, i32) -> Result<(), String>,
{
    let owner_group = exact_owner_process_group(owner, producer)?;
    signal_group(owner_group, libc::SIGSTOP)?;
    wait_for_dedicated_group_stop(owner_group)?;
    let mut system = System::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut executable_identities = HashMap::new();
    let mut previous_inventory: Option<Vec<(u32, u64)>> = None;
    let mut stable_count = 0;
    while Instant::now() < deadline && stable_count < 3 {
        let Some((descendants, mpv)) = discover_descendant_identities(
            &mut system,
            &mut executable_identities,
            owner.pid,
            Some(owner_group),
            Some(producer.pid),
        )?
        else {
            return Err(format!(
                "owner PID {} disappeared while freezing final inventory",
                owner.pid
            ));
        };
        let descendants = merge_live_descendants(&system, last_descendants, descendants);
        let mpv = merge_live_mpv(&system, last_mpv, mpv);
        for identity in &descendants {
            if let Some(process) = system.process(sysinfo::Pid::from_u32(identity.pid))
                && exact_descendant_matches(identity, process)
                && !process.kill_with(Signal::Stop).unwrap_or(false)
            {
                return Err(format!(
                    "failed to freeze exact descendant PID {} before final inventory",
                    identity.pid
                ));
            }
        }
        let inventory = descendants
            .iter()
            .map(|identity| (identity.pid, identity.start_time_unix_s))
            .collect::<Vec<_>>();
        if previous_inventory.as_ref() == Some(&inventory) {
            stable_count += 1;
        } else {
            previous_inventory = Some(inventory);
            stable_count = 1;
        }
        *last_descendants = descendants;
        *last_mpv = mpv;
        thread::sleep(Duration::from_millis(20));
    }
    if stable_count < 3 {
        return Err("final frozen process inventory did not stabilize".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn hard_kill_frozen_descendant(identity: &DescendantIdentity) -> Result<(), String> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    let pid = sysinfo::Pid::from_u32(identity.pid);
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let Some(process) = system.process(pid) else {
        return Ok(());
    };
    if !exact_descendant_matches(identity, process) {
        return Ok(());
    }
    if !process.kill() {
        return Err(format!(
            "failed to hard-kill frozen exact descendant PID {}",
            identity.pid
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if !system
            .process(pid)
            .is_some_and(|process| exact_descendant_matches(identity, process))
        {
            return Ok(());
        }
    }
    Err(format!(
        "frozen exact descendant PID {} survived hard cleanup",
        identity.pid
    ))
}

#[cfg(unix)]
fn capture_stopped_owner_identity(
    child: &mut Child,
    partial: Option<&PartialProcessIdentity>,
) -> Result<LiveProcessIdentity, String> {
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid))
            && let Some(path) = process.exe()
        {
            let process_group_id = process_group_id(pid)?;
            if let Some(partial) = partial
                && (partial.pid != pid
                    || partial.start_time_unix_s != process.start_time()
                    || partial.process_group_id != process_group_id)
            {
                return Err("stopped emergency owner differs from partial startup identity".into());
            }
            let executable = path
                .canonicalize()
                .map_err(|error| format!("canonicalize stopped emergency owner: {error}"))?;
            let executable_bytes = executable
                .metadata()
                .map_err(|error| format!("stat stopped emergency owner: {error}"))?
                .len();
            return Ok(LiveProcessIdentity {
                pid,
                start_time_unix_s: process.start_time(),
                process_group_id,
                executable_sha256: sha256_file(&executable)?,
                executable,
                executable_bytes,
            });
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("query stopped emergency owner: {error}"))?
        {
            return Err(format!(
                "startup owner exited before emergency identity capture: {status}"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out capturing stopped emergency owner PID {pid}"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn validate_owned_child_before_group_signal(
    child: &mut Child,
    partial: Option<&PartialProcessIdentity>,
) -> Result<PartialProcessIdentity, String> {
    if let Some(status) = child
        .try_wait()
        .map_err(|error| format!("query startup owner before process-group freeze: {error}"))?
    {
        return Err(format!(
            "startup owner PID {} already exited before process-group freeze: {status}",
            child.id()
        ));
    }
    let pid = child.id();
    let refresh = ProcessRefreshKind::nothing().without_tasks();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    let process = system.process(sysinfo::Pid::from_u32(pid)).ok_or_else(|| {
        format!("owned startup owner PID {pid} is not observable before process-group freeze")
    })?;
    let current = PartialProcessIdentity {
        pid,
        start_time_unix_s: process.start_time(),
        process_group_id: process_group_id(pid)?,
    };
    let group = current.process_group_id.ok_or_else(|| {
        format!("owned startup owner PID {pid} has no Unix process-group identity")
    })?;
    if group != pid {
        return Err(format!(
            "startup owner PID {pid} is not leader of dedicated group {group}"
        ));
    }
    if partial.is_some_and(|expected| expected != &current) {
        return Err(format!(
            "current startup owner PID/start/PGID differs from partial identity: \
             expected {partial:?}, observed {current:?}"
        ));
    }
    Ok(current)
}

#[cfg(unix)]
fn emergency_shutdown_spawned_tree<F>(
    binary: &Path,
    child: &mut Child,
    producer: &LiveProcessIdentity,
    partial: Option<&PartialProcessIdentity>,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    publish_frozen: F,
) -> Result<LiveProcessIdentity, String>
where
    F: FnOnce(&LiveProcessIdentity, &[MpvIdentity], &[DescendantIdentity]) -> Result<(), String>,
{
    emergency_shutdown_spawned_tree_with_group_signal(
        binary,
        child,
        producer,
        partial,
        last_mpv,
        last_descendants,
        publish_frozen,
        |group, requested_signal| signal_dedicated_owner_group(group, producer, requested_signal),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn emergency_shutdown_spawned_tree_with_group_signal<F, S>(
    binary: &Path,
    child: &mut Child,
    producer: &LiveProcessIdentity,
    partial: Option<&PartialProcessIdentity>,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    publish_frozen: F,
    mut signal_group: S,
) -> Result<LiveProcessIdentity, String>
where
    F: FnOnce(&LiveProcessIdentity, &[MpvIdentity], &[DescendantIdentity]) -> Result<(), String>,
    S: FnMut(u32, i32) -> Result<(), String>,
{
    let observed = validate_owned_child_before_group_signal(child, partial)?;
    let group = observed
        .process_group_id
        .expect("validated Unix startup owner has a process group");
    signal_group(group, libc::SIGSTOP)?;
    wait_for_dedicated_group_stop(group)?;
    let owner = capture_stopped_owner_identity(child, Some(&observed))?;
    shutdown_measured_tree_with_group_signal(
        binary,
        child,
        producer,
        &owner,
        last_mpv,
        last_descendants,
        false,
        |mpv, descendants| publish_frozen(&owner, mpv, descendants),
        &mut signal_group,
    )?;
    Ok(owner)
}

#[cfg(unix)]
fn cleanup_after_startup_error(
    args: &Args,
    producer: &LiveProcessIdentity,
    partial: Option<&PartialProcessIdentity>,
    child: &mut Child,
    original_error: String,
) -> String {
    let mut mpv = Vec::new();
    let mut descendants = Vec::new();
    let cleanup = emergency_shutdown_spawned_tree(
        &args.binary,
        child,
        producer,
        partial,
        &mut mpv,
        &mut descendants,
        |owner, mpv, descendants| {
            write_live_identity(
                &args.identity_file,
                "cleanup_requested",
                producer,
                Some(owner),
                partial,
                mpv,
                descendants,
                false,
            )
        },
    );
    match cleanup {
        Ok(owner) => match write_live_identity(
            &args.identity_file,
            "cleaned",
            producer,
            Some(&owner),
            partial,
            &mpv,
            &descendants,
            true,
        ) {
            Ok(()) => original_error,
            Err(cleanup_error) => {
                format!("{original_error}; emergency cleanup proof failed: {cleanup_error}")
            }
        },
        Err(cleanup_error) => {
            let mut fallback_errors = Vec::new();
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = child.kill() {
                        fallback_errors.push(format!(
                            "hard-kill still-owned startup child PID {}: {error}",
                            child.id()
                        ));
                    }
                    if let Err(error) = child.wait() {
                        fallback_errors.push(format!(
                            "wait for still-owned startup child PID {}: {error}",
                            child.id()
                        ));
                    }
                }
                Err(error) => fallback_errors.push(format!(
                    "query still-owned startup child PID {}: {error}",
                    child.id()
                )),
            }
            for identity in descendants.iter().rev() {
                if let Err(error) = hard_kill_frozen_descendant(identity) {
                    fallback_errors.push(error);
                }
            }
            let fallback = if fallback_errors.is_empty() {
                String::new()
            } else {
                format!(
                    "; exact-PID fallback errors: {}",
                    fallback_errors.join("; ")
                )
            };
            format!(
                "{original_error}; emergency startup cleanup failed without process-group kill: \
                 {cleanup_error}{fallback}"
            )
        }
    }
}

#[cfg(not(unix))]
fn cleanup_after_startup_error(
    _args: &Args,
    _producer: &LiveProcessIdentity,
    _partial: Option<&PartialProcessIdentity>,
    child: &mut Child,
    original_error: String,
) -> String {
    let _ = child.kill();
    let _ = child.wait();
    original_error
}

#[allow(clippy::too_many_arguments)]
#[cfg(unix)]
fn shutdown_measured_tree<F>(
    binary: &Path,
    child: &mut Child,
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    require_mpv_identity: bool,
    publish_frozen: F,
) -> Result<(), String>
where
    F: FnOnce(&[MpvIdentity], &[DescendantIdentity]) -> Result<(), String>,
{
    shutdown_measured_tree_with_group_signal(
        binary,
        child,
        producer,
        owner,
        last_mpv,
        last_descendants,
        require_mpv_identity,
        publish_frozen,
        |group, requested_signal| signal_dedicated_owner_group(group, producer, requested_signal),
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(not(unix))]
fn shutdown_measured_tree<F>(
    binary: &Path,
    child: &mut Child,
    _producer: &LiveProcessIdentity,
    _owner: &LiveProcessIdentity,
    last_mpv: &mut [MpvIdentity],
    last_descendants: &mut [DescendantIdentity],
    require_mpv_identity: bool,
    publish_frozen: F,
) -> Result<(), String>
where
    F: FnOnce(&[MpvIdentity], &[DescendantIdentity]) -> Result<(), String>,
{
    publish_frozen(last_mpv, last_descendants)?;
    shutdown_child(
        binary,
        child,
        last_mpv,
        last_descendants,
        require_mpv_identity,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn shutdown_measured_tree_with_group_signal<F, S>(
    _binary: &Path,
    child: &mut Child,
    producer: &LiveProcessIdentity,
    owner: &LiveProcessIdentity,
    last_mpv: &mut Vec<MpvIdentity>,
    last_descendants: &mut Vec<DescendantIdentity>,
    require_mpv_identity: bool,
    publish_frozen: F,
    mut signal_group: S,
) -> Result<(), String>
where
    F: FnOnce(&[MpvIdentity], &[DescendantIdentity]) -> Result<(), String>,
    S: FnMut(u32, i32) -> Result<(), String>,
{
    let mut errors = Vec::new();
    let group_frozen = match freeze_final_process_inventory(
        producer,
        owner,
        last_mpv,
        last_descendants,
        &mut signal_group,
    ) {
        Ok(()) => true,
        Err(error) => {
            errors.push(error);
            false
        }
    };
    if require_mpv_identity && last_mpv.is_empty() {
        errors.push(
            "final frozen inventory had no exact mpv PID/start-time/IPC argv identity".to_string(),
        );
    }
    if let Err(error) = publish_frozen(last_mpv, last_descendants) {
        errors.push(error);
    }
    if group_frozen {
        match exact_owner_process_group(owner, producer) {
            Ok(group) => {
                if let Err(error) = signal_group(group, libc::SIGKILL) {
                    errors.push(error);
                }
            }
            Err(error) => errors.push(format!(
                "refused dedicated process-group kill after exact owner revalidation: {error}"
            )),
        }
    }
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            if let Err(error) = child.wait() {
                errors.push(format!(
                    "wait for frozen exact ytt child PID {}: {error}",
                    child.id()
                ));
            }
        }
        Err(error) => errors.push(format!("query frozen ytt child: {error}")),
    }
    for identity in last_descendants.iter().rev() {
        if let Err(error) = hard_kill_frozen_descendant(identity) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(not(unix))]
fn shutdown_child(
    binary: &Path,
    child: &mut Child,
    last_mpv_identities: &[MpvIdentity],
    last_descendant_identities: &[DescendantIdentity],
    require_mpv_identity: bool,
) -> Result<(), String> {
    shutdown_child_with_grace(
        binary,
        child,
        last_mpv_identities,
        last_descendant_identities,
        require_mpv_identity,
        Duration::from_secs(5),
    )
}

#[cfg(any(test, not(unix)))]
fn shutdown_child_with_grace(
    binary: &Path,
    child: &mut Child,
    last_mpv_identities: &[MpvIdentity],
    last_descendant_identities: &[DescendantIdentity],
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
    let mpv_pids = last_mpv_identities
        .iter()
        .map(|identity| identity.pid)
        .collect::<HashSet<_>>();
    for identity in last_descendant_identities {
        if !mpv_pids.contains(&identity.pid) {
            stop_exact_descendant(identity, hard_fallback)?;
        }
    }
    Ok(())
}

#[cfg(any(test, not(unix)))]
fn stop_exact_mpv(identity: &MpvIdentity, hard_fallback: bool) -> Result<(), String> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
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
                mpv_executable_matches(identity, process)
                    && exact_mpv_identity_matches(
                        identity,
                        identity.pid,
                        process.start_time(),
                        &command,
                    )
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
            mpv_executable_matches(identity, process)
                && exact_mpv_identity_matches(
                    identity,
                    identity.pid,
                    process.start_time(),
                    &command,
                )
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
    if !mpv_executable_matches(identity, process)
        || !exact_mpv_identity_matches(identity, identity.pid, process.start_time(), &command)
    {
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
            mpv_executable_matches(identity, process)
                && exact_mpv_identity_matches(
                    identity,
                    identity.pid,
                    process.start_time(),
                    &command,
                )
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

#[cfg(any(test, not(unix)))]
fn stop_exact_descendant(identity: &DescendantIdentity, hard_fallback: bool) -> Result<(), String> {
    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .without_tasks();
    let mut system = System::new();
    let pid = sysinfo::Pid::from_u32(identity.pid);
    let natural_deadline = Instant::now()
        + if hard_fallback {
            Duration::ZERO
        } else {
            Duration::from_secs(2)
        };
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if !system
            .process(pid)
            .is_some_and(|process| exact_descendant_matches(identity, process))
        {
            return Ok(());
        }
        if Instant::now() >= natural_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let process = system.process(pid).ok_or_else(|| {
        format!(
            "exact descendant PID {} disappeared during cleanup",
            identity.pid
        )
    })?;
    if !process.kill_with(Signal::Term).unwrap_or(false) && !process.kill() {
        return Err(format!(
            "failed to terminate exact descendant PID {} after identity verification",
            identity.pid
        ));
    }
    let term_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < term_deadline {
        thread::sleep(Duration::from_millis(50));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if !system
            .process(pid)
            .is_some_and(|process| exact_descendant_matches(identity, process))
        {
            return Ok(());
        }
    }
    let Some(process) = system.process(pid) else {
        return Ok(());
    };
    if !exact_descendant_matches(identity, process) {
        return Ok(());
    }
    if !process.kill() {
        return Err(format!(
            "failed to hard-kill exact descendant PID {} after TERM grace",
            identity.pid
        ));
    }
    let kill_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < kill_deadline {
        thread::sleep(Duration::from_millis(50));
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        if !system
            .process(pid)
            .is_some_and(|process| exact_descendant_matches(identity, process))
        {
            return Ok(());
        }
    }
    Err(format!(
        "exact descendant PID {} remained after verified cleanup and wait",
        identity.pid
    ))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Child;
    use std::sync::mpsc;

    use super::{
        Aggregate, Args, LiveProcessIdentity, MeasuredMpvProof, MpvIdentity, RoleSample,
        cpu_interval_overlap, emergency_shutdown_spawned_tree,
        emergency_shutdown_spawned_tree_with_group_signal, exact_mpv_identity_matches,
        header_record, input_ipc_server_argv, live_identity_update_is_stale,
        producer_binary_sha256, sha256_file, shutdown_child_with_grace, shutdown_measured_tree,
        shutdown_measured_tree_with_group_signal, silent_mpv_args, stop_exact_mpv,
        wait_for_partial_process_identity, wait_for_process_identity,
    };
    use yututui::api::{Song, validate_playable_url, validate_playback_target_for_handoff};
    use yututui::search_source::SearchSource;

    struct TestChildGuard(Option<Child>);

    impl TestChildGuard {
        fn new(child: Child) -> Self {
            Self(Some(child))
        }

        fn child_mut(&mut self) -> &mut Child {
            self.0.as_mut().expect("test child is present")
        }

        fn take(&mut self) -> Child {
            self.0.take().expect("test child is present")
        }
    }

    impl Drop for TestChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }

    #[cfg(unix)]
    fn spawn_group_signal_sentinel() -> TestChildGuard {
        let current_exe = std::env::current_exe().expect("test executable");
        let mut command = std::process::Command::new(current_exe);
        command
            .args(["--ignored", "--exact", "tests::cleanup_wait_helper_process"])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        TestChildGuard::new(
            command
                .spawn()
                .expect("spawn dedicated group-signal sentinel"),
        )
    }

    struct TestWaiterGuard {
        stop: mpsc::Sender<()>,
        waiter: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    }

    impl TestWaiterGuard {
        fn new(mut child: Child) -> Self {
            let (stop, requests) = mpsc::channel();
            let waiter = std::thread::spawn(move || {
                loop {
                    if let Some(status) = child.try_wait()? {
                        return Ok(status);
                    }
                    if requests.try_recv().is_ok() {
                        let _ = child.kill();
                        return child.wait();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            Self {
                stop,
                waiter: Some(waiter),
            }
        }

        fn join(mut self) -> std::io::Result<std::process::ExitStatus> {
            self.waiter
                .take()
                .expect("test waiter is present")
                .join()
                .expect("test waiter thread")
        }
    }

    impl Drop for TestWaiterGuard {
        fn drop(&mut self) {
            if let Some(waiter) = self.waiter.take() {
                let _ = self.stop.send(());
                let _ = waiter.join();
            }
        }
    }

    #[cfg(unix)]
    struct LateProcessGuard(PathBuf);

    #[cfg(unix)]
    impl Drop for LateProcessGuard {
        fn drop(&mut self) {
            let Ok(pid) = std::fs::read_to_string(&self.0)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .ok_or(())
            else {
                return;
            };
            let refresh = sysinfo::ProcessRefreshKind::nothing()
                .with_cmd(sysinfo::UpdateKind::Always)
                .without_tasks();
            let mut system = sysinfo::System::new();
            system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);
            if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
                let command = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if command.iter().any(|part| part == &self.0.to_string_lossy()) {
                    let _ = process.kill();
                }
            }
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn capture_test_mpv_identity(child: &mut Child) -> Result<MpvIdentity, String> {
        let pid = child.id();
        let expected_executable =
            std::env::current_exe().map_err(|error| format!("resolve test executable: {error}"))?;
        let expected_sha256 = sha256_file(&expected_executable)?;
        let live_identity = wait_for_process_identity(pid, &expected_sha256, child)?;
        let refresh = sysinfo::ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::Always)
            .with_exe(sysinfo::UpdateKind::Always)
            .without_tasks();
        let mut system = sysinfo::System::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh);
            if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
                let command = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if let (Some(input_ipc_server_argv), Some(executable)) = (
                    input_ipc_server_argv(&command),
                    process.exe().and_then(|path| path.canonicalize().ok()),
                ) && process.start_time() == live_identity.start_time_unix_s
                    && executable == live_identity.executable
                {
                    return Ok(MpvIdentity {
                        pid,
                        start_time_unix_s: live_identity.start_time_unix_s,
                        executable: live_identity.executable,
                        executable_bytes: live_identity.executable_bytes,
                        executable_sha256: live_identity.executable_sha256,
                        input_ipc_server_argv,
                    });
                }
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("query test helper while capturing identity: {error}"))?
            {
                return Err(format!(
                    "test helper exited before publishing its exact identity: {status}"
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out capturing exact IPC/executable identity for test helper PID {pid}"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn header_records_the_current_sampler_binary_sha256() {
        let executable = std::env::current_exe().expect("resolve current test executable");
        let expected = sha256_file(&executable).expect("hash current test executable");
        let producer_hash = producer_binary_sha256().expect("hash current sampler executable");
        let args = Args {
            output: PathBuf::from("samples.ndjson"),
            pid_file: None,
            identity_file: PathBuf::from("process-identity.json"),
            controller_ready_file: None,
            binary: PathBuf::from("ytt"),
            child_args: Vec::new(),
            warmup: std::time::Duration::from_secs(1),
            duration: std::time::Duration::from_secs(2),
            interval: std::time::Duration::from_millis(500),
            require_silent_mpv: false,
        };
        let header = header_record(
            &args,
            42,
            "measured-hash",
            &producer_hash,
            "self-test-run",
            123,
            (100, 30),
        );

        assert_eq!(producer_hash, expected);
        assert_eq!(producer_hash.len(), 64);
        assert_eq!(header["kind"], "header");
        assert_eq!(header["binary_sha256"], "measured-hash");
        assert_eq!(header["producer_binary_sha256"], producer_hash);
        assert_eq!(header["run_id"], "self-test-run");
        assert_eq!(header["terminal_geometry"], serde_json::json!([100, 30]));
    }

    #[test]
    fn cpu_summary_clamps_and_time_weights_raw_intervals() {
        let window_start = std::time::Duration::from_secs(1);
        let window_end = std::time::Duration::from_secs(3);
        assert_eq!(
            cpu_interval_overlap(
                Some(std::time::Duration::from_millis(200)),
                std::time::Duration::from_millis(1_200),
                window_start,
                window_end,
            ),
            std::time::Duration::from_millis(200),
            "the warmup portion of a crossing interval must be excluded"
        );
        assert_eq!(
            cpu_interval_overlap(
                Some(std::time::Duration::from_millis(2_100)),
                std::time::Duration::from_millis(3_400),
                window_start,
                window_end,
            ),
            std::time::Duration::from_millis(900),
            "the post-window portion of the final interval must be excluded"
        );
        assert_eq!(
            cpu_interval_overlap(
                Some(std::time::Duration::from_millis(3_400)),
                std::time::Duration::from_millis(4_000),
                window_start,
                window_end,
            ),
            std::time::Duration::ZERO
        );
        assert_eq!(
            cpu_interval_overlap(
                None,
                std::time::Duration::from_millis(500),
                window_start,
                window_end,
            ),
            std::time::Duration::ZERO
        );

        let mut aggregate = Aggregate::default();
        for (cpu_percent, overlap_ms) in [(10.0, 200), (20.0, 800), (30.0, 1_000)] {
            aggregate.push(
                RoleSample {
                    processes: 1,
                    cpu_percent,
                    rss_bytes: 100,
                },
                std::time::Duration::from_millis(overlap_ms),
            );
        }
        let summary = aggregate.json(std::time::Duration::from_secs(2));
        assert_eq!(summary["samples"], 3);
        assert_eq!(summary["mean_rss_bytes"], 100);
        assert_eq!(summary["peak_rss_bytes"], 100);
        let mean_cpu = summary["mean_cpu_percent"]
            .as_f64()
            .expect("mean CPU is numeric");
        assert!((mean_cpu - 24.0).abs() < 1e-12);
        assert_ne!(
            mean_cpu, 20.0,
            "jittered intervals must not be equally weighted"
        );
    }

    #[test]
    fn stale_startup_updates_cannot_overwrite_external_cleanup_state() {
        assert!(!live_identity_update_is_stale("owner_starting", "startup"));
        assert!(!live_identity_update_is_stale("running", "owner_starting"));
        assert!(live_identity_update_is_stale(
            "owner_starting",
            "cleanup_requested"
        ));
        assert!(live_identity_update_is_stale("running", "cleaned"));
    }

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
            executable: PathBuf::from("mpv"),
            executable_bytes: 1,
            executable_sha256: "00".repeat(32),
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
        let child = std::process::Command::new(std::env::current_exe().expect("test exe"))
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
        let mut child = TestChildGuard::new(child);
        let identity = capture_test_mpv_identity(child.child_mut())
            .expect("cleanup helper publishes its exact identity");

        // Reap the direct test child concurrently. In production mpv is ytt's descendant and
        // is reaped by the OS after ytt is gone; this preserves the same observable lifecycle.
        let waiter = TestWaiterGuard::new(child.take());
        stop_exact_mpv(&identity, true).expect("verified cleanup succeeds");
        let status = waiter.join().expect("wait cleanup helper");
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
        let ytt = std::process::Command::new(&current_exe)
            .args(helper_args)
            .arg("ytt-owner-helper")
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ytt owner helper");
        let mut ytt = TestChildGuard::new(ytt);
        let ipc_arg = format!(
            "--input-ipc-server={}/ytt-perf-fallback-{}-{}.sock",
            std::env::temp_dir().display(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        );
        let mpv = std::process::Command::new(&current_exe)
            .args(helper_args)
            .arg(&ipc_arg)
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn mpv helper");
        let mut mpv = TestChildGuard::new(mpv);
        let identity = capture_test_mpv_identity(mpv.child_mut())
            .expect("mpv helper publishes its exact identity");
        let mpv_waiter = TestWaiterGuard::new(mpv.take());

        shutdown_child_with_grace(
            &current_exe,
            ytt.child_mut(),
            &[identity],
            &[],
            true,
            std::time::Duration::from_millis(20),
        )
        .expect("hard fallback cleans exact child and mpv");
        assert!(
            ytt.child_mut()
                .try_wait()
                .expect("query ytt helper")
                .is_some(),
            "exact ytt child must have been reaped"
        );
        let mpv_status = mpv_waiter.join().expect("wait mpv helper");
        assert!(!mpv_status.success(), "exact mpv helper must be terminated");
    }

    #[cfg(unix)]
    #[test]
    fn final_frozen_inventory_cleans_a_late_setsid_term_ignoring_child() {
        let current_exe = std::env::current_exe().expect("test exe");
        let late_pid_file = std::env::temp_dir().join(format!(
            "ytt-perf-late-child-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let late_guard = LateProcessGuard(late_pid_file.clone());
        let mut command = std::process::Command::new(&current_exe);
        command
            .args([
                "--ignored",
                "--exact",
                "tests::cleanup_wait_helper_process",
                "--",
                "late-owner",
            ])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .env("TUI_PERF_LATE_CHILD_PID_FILE", &late_pid_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let owner = command.spawn().expect("spawn late-child owner helper");
        let mut owner = TestChildGuard::new(owner);
        let expected_hash = sha256_file(&current_exe).expect("hash owner helper");
        let owner_identity =
            wait_for_process_identity(owner.child_mut().id(), &expected_hash, owner.child_mut())
                .expect("capture owner identity");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&late_pid_file)
            .is_ok_and(|contents| !contents.trim().is_empty())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "late setsid child did not publish its PID"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let late_pid = std::fs::read_to_string(&late_pid_file)
            .expect("read late PID")
            .trim()
            .parse::<u32>()
            .expect("parse late PID");
        let producer_identity = super::current_process_identity().expect("producer identity");
        let mut mpv = Vec::new();
        let mut descendants = Vec::new();
        shutdown_measured_tree(
            &current_exe,
            owner.child_mut(),
            &producer_identity,
            &owner_identity,
            &mut mpv,
            &mut descendants,
            false,
            |_mpv, _descendants| Ok(()),
        )
        .expect("frozen final inventory cleans the full late tree");
        assert!(
            descendants.iter().any(|identity| identity.pid == late_pid),
            "late setsid child must be captured in the exact final inventory"
        );
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        assert!(
            system.process(sysinfo::Pid::from_u32(late_pid)).is_none(),
            "late setsid child survived frozen exact cleanup"
        );
        drop(late_guard);
    }

    #[cfg(unix)]
    #[test]
    fn missing_exact_owner_never_reuses_recorded_process_group_for_cleanup() {
        let current_exe = std::env::current_exe()
            .expect("test executable")
            .canonicalize()
            .expect("canonical test executable");
        let mut command = std::process::Command::new(&current_exe);
        command
            .args(["--ignored", "--exact", "tests::cleanup_wait_helper_process"])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command.spawn().expect("spawn sentinel cleanup child");
        let mut child = TestChildGuard::new(child);
        let producer = super::current_process_identity().expect("producer identity");
        let missing_pid = u32::MAX;
        let missing_owner = LiveProcessIdentity {
            pid: missing_pid,
            start_time_unix_s: 1,
            process_group_id: Some(missing_pid),
            executable_bytes: current_exe.metadata().expect("test metadata").len(),
            executable_sha256: sha256_file(&current_exe).expect("test executable hash"),
            executable: current_exe.clone(),
        };
        let mut mpv = Vec::new();
        let mut descendants = Vec::new();
        let mut group_signals = Vec::new();
        let error = shutdown_measured_tree_with_group_signal(
            &current_exe,
            child.child_mut(),
            &producer,
            &missing_owner,
            &mut mpv,
            &mut descendants,
            false,
            |_mpv, _descendants| Ok(()),
            |group, requested_signal| {
                group_signals.push((group, requested_signal));
                Ok(())
            },
        )
        .expect_err("missing exact owner must fail closed");
        assert!(
            error.contains("exact owner PID") && error.contains("no longer present"),
            "unexpected fail-closed error: {error}"
        );
        assert!(
            group_signals.is_empty(),
            "missing owner must not signal a reusable numeric process group: {group_signals:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reaped_startup_child_never_signals_derived_process_group() {
        let current_exe = std::env::current_exe().expect("test executable");
        let mut command = std::process::Command::new(&current_exe);
        command
            .args(["--ignored", "--exact", "tests::cleanup_wait_helper_process"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut reaped_child = command.spawn().expect("spawn fast startup child");
        assert!(
            reaped_child
                .wait()
                .expect("reap fast startup child")
                .success(),
            "fast startup child must exit cleanly"
        );
        let mut sentinel = spawn_group_signal_sentinel();
        let sentinel_group = sentinel.child_mut().id();
        let sentinel_group_i32 = i32::try_from(sentinel_group).expect("sentinel PGID fits pid_t");
        let producer = super::current_process_identity().expect("producer identity");
        let mut mpv = Vec::new();
        let mut descendants = Vec::new();
        let mut group_signals = Vec::new();
        let error = emergency_shutdown_spawned_tree_with_group_signal(
            &current_exe,
            &mut reaped_child,
            &producer,
            None,
            &mut mpv,
            &mut descendants,
            |_owner, _mpv, _descendants| Ok(()),
            |group, requested_signal| {
                group_signals.push((group, requested_signal));
                // SAFETY: this PGID belongs to the dedicated sentinel spawned by this test.
                let _ = unsafe { libc::kill(-sentinel_group_i32, libc::SIGKILL) };
                Ok(())
            },
        )
        .expect_err("already-reaped startup child must fail closed");
        let sentinel_status = sentinel
            .child_mut()
            .try_wait()
            .expect("query group-signal sentinel");
        assert!(
            error.contains("already exited before process-group freeze"),
            "unexpected fail-closed error: {error}"
        );
        assert!(
            group_signals.is_empty(),
            "reaped child must not trigger a numeric group signal: {group_signals:?}"
        );
        assert!(
            sentinel_status.is_none(),
            "unrelated group-signal sentinel was terminated"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_partial_startup_identity_never_signals_process_group() {
        let current_exe = std::env::current_exe().expect("test executable");
        let mut owner_command = std::process::Command::new(&current_exe);
        owner_command
            .args(["--ignored", "--exact", "tests::cleanup_wait_helper_process"])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let owner = owner_command.spawn().expect("spawn startup owner sentinel");
        let mut owner = TestChildGuard::new(owner);
        let mut stale_partial =
            wait_for_partial_process_identity(owner.child_mut().id(), owner.child_mut())
                .expect("capture startup owner partial identity");
        stale_partial.start_time_unix_s = stale_partial.start_time_unix_s.saturating_add(1);
        let mut sentinel = spawn_group_signal_sentinel();
        let sentinel_group = sentinel.child_mut().id();
        let sentinel_group_i32 = i32::try_from(sentinel_group).expect("sentinel PGID fits pid_t");
        let producer = super::current_process_identity().expect("producer identity");
        let mut mpv = Vec::new();
        let mut descendants = Vec::new();
        let mut group_signals = Vec::new();
        let error = emergency_shutdown_spawned_tree_with_group_signal(
            &current_exe,
            owner.child_mut(),
            &producer,
            Some(&stale_partial),
            &mut mpv,
            &mut descendants,
            |_owner, _mpv, _descendants| Ok(()),
            |group, requested_signal| {
                group_signals.push((group, requested_signal));
                // SAFETY: this PGID belongs to the dedicated sentinel spawned by this test.
                let _ = unsafe { libc::kill(-sentinel_group_i32, libc::SIGKILL) };
                Ok(())
            },
        )
        .expect_err("stale partial startup identity must fail closed");
        let sentinel_status = sentinel
            .child_mut()
            .try_wait()
            .expect("query group-signal sentinel");
        assert!(
            error.contains("differs from partial identity"),
            "unexpected fail-closed error: {error}"
        );
        assert!(
            group_signals.is_empty(),
            "stale partial identity must not trigger a group signal: {group_signals:?}"
        );
        assert!(
            sentinel_status.is_none(),
            "unrelated group-signal sentinel was terminated"
        );
        assert!(
            owner
                .child_mut()
                .try_wait()
                .expect("query still-owned startup child")
                .is_none(),
            "stale partial validation must not terminate the still-owned child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_error_emergency_cleanup_captures_late_setsid_child() {
        let current_exe = std::env::current_exe().expect("test exe");
        let late_pid_file = std::env::temp_dir().join(format!(
            "ytt-perf-startup-error-child-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let late_guard = LateProcessGuard(late_pid_file.clone());
        let mut command = std::process::Command::new(&current_exe);
        command
            .args([
                "--ignored",
                "--exact",
                "tests::cleanup_wait_helper_process",
                "--",
                "startup-error-owner",
            ])
            .env("TUI_PERF_CLEANUP_HELPER", "1")
            .env("TUI_PERF_LATE_CHILD_PID_FILE", &late_pid_file)
            .env("TUI_PERF_LATE_CHILD_DELAY_MS", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let owner = command.spawn().expect("spawn startup-error owner helper");
        let mut owner = TestChildGuard::new(owner);
        let partial = wait_for_partial_process_identity(owner.child_mut().id(), owner.child_mut())
            .expect("capture partial startup owner");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&late_pid_file)
            .is_ok_and(|contents| !contents.trim().is_empty())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "startup-error setsid child did not publish its PID"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let late_pid = std::fs::read_to_string(&late_pid_file)
            .expect("read startup-error late PID")
            .trim()
            .parse::<u32>()
            .expect("parse startup-error late PID");
        let producer_identity = super::current_process_identity().expect("producer identity");
        let mut mpv = Vec::new();
        let mut descendants = Vec::new();
        let _owner_identity = emergency_shutdown_spawned_tree(
            &current_exe,
            owner.child_mut(),
            &producer_identity,
            Some(&partial),
            &mut mpv,
            &mut descendants,
            |_owner, _mpv, frozen_descendants| {
                if !frozen_descendants
                    .iter()
                    .any(|identity| identity.pid == late_pid)
                {
                    return Err("forced startup error missed late setsid child".to_string());
                }
                Ok(())
            },
        )
        .expect("forced startup error uses emergency frozen cleanup");
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        assert!(
            system.process(sysinfo::Pid::from_u32(late_pid)).is_none(),
            "startup-error setsid child survived emergency cleanup"
        );
        drop(late_guard);
    }

    #[test]
    #[ignore]
    fn cleanup_wait_helper_process() {
        if std::env::var_os("TUI_PERF_CLEANUP_HELPER").is_some() {
            if let Some(path) = std::env::var_os("TUI_PERF_LATE_CHILD_PID_FILE") {
                let delay_ms = std::env::var("TUI_PERF_LATE_CHILD_DELAY_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(100);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                let script = r#"import os,signal,sys,time
os.setsid()
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(sys.argv[1], 'w', encoding='ascii') as stream:
    stream.write(str(os.getpid()))
    stream.flush()
while True:
    time.sleep(1)
"#;
                let _late_child = std::process::Command::new("python3")
                    .args(["-c", script])
                    .arg(path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn late setsid child");
            }
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
