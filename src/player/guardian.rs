//! A same-binary guardian which makes every mpv lifetime fail closed.
//!
//! The owner first starts `ytt __mpv-guardian` blocked on a private request pipe. Only after
//! Unix process-group ownership or both Windows Job Objects are armed does it send the request.
//! The guardian then owns mpv, a heartbeat lease, and (on POSIX) an mpv-native `fd://` IPC lease.
//! Losing any owner/guardian boundary terminates mpv and every helper it launched.

use std::io::{BufRead, Read, Write};
use std::process::{Child, Stdio};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::util::process::{self, ProcessProfile};
use crate::util::process_guard::ChildTreeGuard;

const REQUEST_MAX: usize = 1024 * 1024;
const RESPONSE_MAX: usize = 4 * 1024 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(3);
const RUNTIME_PULSE_POLL: Duration = Duration::from_millis(100);
const CHILD_POLL: Duration = Duration::from_millis(50);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROBE_OUTPUT: usize = 1024 * 1024;
const PROBE_OUTPUT_JOIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Set only by the hidden guardian's minimal SIGTERM handler. The owner sends SIGTERM instead of
/// killing the guardian so the process which owns mpv always remains alive long enough to reap it.
#[cfg(unix)]
static GUARDIAN_TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Serialize, Deserialize)]
struct GuardianRequest {
    program: String,
    args: Vec<String>,
    detached: bool,
    mode: GuardianMode,
    /// A handle valid in the guardian process. Windows requires the guardian-only inner Job;
    /// Unix uses a private fd lease instead and leaves this empty.
    windows_inner_job: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum GuardianMode {
    LongLived,
    Probe { timeout_ms: u64, stdout_max: usize },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum GuardianResponse {
    Ready { mpv_pid: u32 },
    Probe { success: bool, stdout: Vec<u8> },
    Error { message: String },
}

#[derive(Debug)]
enum LeaseEvent {
    Heartbeat,
    Closed,
}

/// mpv's input IPC client is bidirectional and may emit events even though the guardian sends no
/// commands. Drain continuously so a long session can never fill the socket buffer and stall mpv.
#[cfg(unix)]
struct NativeMpvLease {
    control: std::os::unix::net::UnixStream,
    drain: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl NativeMpvLease {
    fn new(mut stream: std::os::unix::net::UnixStream) -> Result<Self> {
        let control = stream.try_clone().context("clone mpv native IPC lease")?;
        let drain = std::thread::Builder::new()
            .name("ytt-mpv-ipc-drain".to_owned())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
            })
            .context("start mpv native IPC drain")?;
        Ok(Self {
            control,
            drain: Some(drain),
        })
    }

    fn close(&mut self) {
        let _ = self.control.shutdown(std::net::Shutdown::Both);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

#[cfg(unix)]
impl Drop for NativeMpvLease {
    fn drop(&mut self) {
        self.close();
    }
}

/// Own a spawned child from the instant it exists until its status has been synchronously reaped.
///
/// Every error after `Command::spawn` unwinds through this guard. That includes a broken Ready
/// response pipe, a failed output reader, and an unexpected supervision error: none may hand an
/// exited mpv (or a blocked guardian during owner-side bootstrap) to container PID 1.
struct ProtectedChild {
    // Terminate the group/Job before the Child handle can be dropped.
    tree: Option<ChildTreeGuard>,
    child: Option<Child>,
}

impl ProtectedChild {
    fn new(child: Child, profile: ProcessProfile) -> Self {
        let tree = ChildTreeGuard::for_std(&child, profile);
        Self {
            tree: Some(tree),
            child: Some(child),
        }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("protected child").id()
    }

    fn child(&self) -> &Child {
        self.child.as_ref().expect("protected child")
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("protected child")
    }

    fn tree(&self) -> &ChildTreeGuard {
        self.tree.as_ref().expect("protected child tree")
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child_mut().stdout.take()
    }

    fn into_parts(mut self) -> (ChildTreeGuard, Child) {
        (
            self.tree.take().expect("protected child tree"),
            self.child.take().expect("protected child"),
        )
    }

    fn reap_exited(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(tree) = self.tree.as_mut() {
            // The direct child still owns its pid/process object, so killing its group/Job here
            // cannot target a reused generation and also removes any helper it left behind.
            tree.terminate();
        }
        let status = self.child_mut().wait()?;
        self.child = None;
        Ok(status)
    }

    fn terminate_and_wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(tree) = self.tree.as_mut() {
            tree.terminate();
        }
        let child = self.child_mut();
        let _ = child.kill();
        let status = child.wait()?;
        self.child = None;
        Ok(status)
    }

    fn is_reaped(&self) -> bool {
        self.child.is_none()
    }
}

impl Drop for ProtectedChild {
    fn drop(&mut self) {
        if self.child.is_some()
            && let Err(error) = self.terminate_and_wait()
        {
            tracing::warn!(%error, "failed to reap protected child during error cleanup");
        }
    }
}

/// Bounded output from a probe executed under the same guardian boundary as playback.
pub(crate) struct ProbeOutput {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
}

/// The private heartbeat writer plus the fixed-capacity kill-all registry slot.
///
/// Dropping this value closes the request pipe. The guardian treats that EOF as an immediate
/// teardown request; its native mpv lease and process-tree guard cover forced guardian death.
pub(crate) struct GuardianLease {
    heartbeat: Option<HeartbeatOwner>,
    registration: Option<super::lifetime::MediaPidRegistration>,
    disk_registration: Option<super::lifetime::DiskLifelineRegistration>,
}

enum HeartbeatOwner {
    Runtime {
        stop: mpsc::Sender<()>,
        writer_thread: std::thread::JoinHandle<()>,
        pulse_task: tokio::task::JoinHandle<()>,
    },
    BoundedProbe {
        stop: mpsc::Sender<()>,
        thread: std::thread::JoinHandle<()>,
    },
}

impl GuardianLease {
    fn new_runtime(
        writer: std::process::ChildStdin,
        request_frame: Vec<u8>,
        registration: super::lifetime::MediaPidRegistration,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current()
            .context("long-lived mpv guardian requires the owner Tokio runtime")?;
        let pulse = Arc::new(AtomicU64::new(1));
        let writer_pulse = Arc::clone(&pulse);
        let (stop, stopped) = mpsc::channel();
        let writer_thread = std::thread::Builder::new()
            .name("ytt-mpv-runtime-heartbeat".to_owned())
            .spawn(move || {
                runtime_heartbeat_writer(
                    writer,
                    request_frame,
                    stopped,
                    writer_pulse,
                    HEARTBEAT_TIMEOUT,
                )
            })
            .context("start runtime-gated mpv heartbeat writer")?;
        let pulse_task = runtime.spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                pulse.fetch_add(1, Ordering::Release);
            }
        });
        Ok(Self {
            heartbeat: Some(HeartbeatOwner::Runtime {
                stop,
                writer_thread,
                pulse_task,
            }),
            registration: Some(registration),
            disk_registration: None,
        })
    }

    /// Probes are already strictly bounded; a tiny OS thread keeps their owner lease alive while
    /// the synchronous caller waits for the guardian response without blocking a runtime worker.
    fn new_probe(
        writer: std::process::ChildStdin,
        request_frame: Vec<u8>,
        registration: super::lifetime::MediaPidRegistration,
    ) -> Result<Self> {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ytt-mpv-probe-heartbeat".to_owned())
            .spawn(move || heartbeat_writer(writer, request_frame, stopped))
            .context("start bounded mpv probe heartbeat")?;
        Ok(Self {
            heartbeat: Some(HeartbeatOwner::BoundedProbe { stop, thread }),
            registration: Some(registration),
            disk_registration: None,
        })
    }

    fn stop_heartbeat(&mut self) {
        match self.heartbeat.take() {
            Some(HeartbeatOwner::Runtime {
                stop,
                writer_thread,
                pulse_task,
            }) => {
                let _ = stop.send(());
                // Never let a pathological blocked request write pin owner teardown. Detaching is
                // safe: guardian termination closes the pipe and unblocks the writer independently.
                drop(writer_thread);
                pulse_task.abort();
            }
            Some(HeartbeatOwner::BoundedProbe { stop, thread }) => {
                let _ = stop.send(());
                drop(thread);
            }
            None => {}
        }
    }

    /// Close the private owner pipe and let the guardian terminate mpv and observe its exit.
    pub(crate) fn request_shutdown(&mut self) {
        self.stop_heartbeat();
    }

    /// Emergency fallback: request the stable guardian boundary to stop and retain the exact disk
    /// marker. Unix registration teardown is cooperative SIGTERM; Windows uses its nested Job.
    /// A later startup can prove whether cleanup completed without trusting a reused pid.
    pub(crate) fn hard_kill_preserving_disk(&mut self) {
        self.stop_heartbeat();
        if let Some(registration) = self.registration.take() {
            registration.terminate_now();
        }
        if let Some(disk_registration) = self.disk_registration.take() {
            std::mem::forget(disk_registration);
        }
    }

    /// Guardian completion proves that mpv is no longer live. Remove PID/disk backstops without
    /// sending another kill that could hit a rapidly reused pid. The guardian has already reaped
    /// mpv itself; the owner only needs to reap the guardian.
    pub(crate) fn disarm_after_guardian_exit(&mut self, clean: bool) {
        self.registration = None;
        if clean {
            self.disk_registration = None;
        } else if let Some(disk_registration) = self.disk_registration.take() {
            std::mem::forget(disk_registration);
        }
        self.stop_heartbeat();
    }

    /// Remove every raw-pid hook before an unavoidable direct `wait()` while retaining the disk
    /// marker. Used only if non-reaping guardian observation itself fails on Unix: the owner then
    /// waits synchronously, so the guardian remains mpv's parent even without the emergency slot.
    #[cfg(unix)]
    fn disarm_before_unobserved_wait(&mut self) {
        self.registration = None;
        if let Some(disk_registration) = self.disk_registration.take() {
            std::mem::forget(disk_registration);
        }
        self.stop_heartbeat();
    }
}

impl Drop for GuardianLease {
    fn drop(&mut self) {
        self.hard_kill_preserving_disk();
    }
}

/// Pieces owned by the audio Mpv guard or an overlay OwnedProcessTree.
pub(crate) struct GuardedSpawn {
    child_tree: Option<ChildTreeGuard>,
    child: Option<Child>,
    lease: Option<GuardianLease>,
    pub(crate) mpv_pid: u32,
}

impl GuardedSpawn {
    pub(crate) fn into_parts(mut self) -> (ChildTreeGuard, Child, GuardianLease, u32) {
        (
            self.child_tree.take().expect("guarded child tree"),
            self.child.take().expect("guarded guardian process"),
            self.lease.take().expect("guarded owner lease"),
            self.mpv_pid,
        )
    }
}

impl Drop for GuardedSpawn {
    fn drop(&mut self) {
        let mut lease = self.lease.take();
        if let (Some(lease), Some(child), Some(tree)) =
            (lease.as_mut(), self.child.take(), self.child_tree.as_mut())
        {
            let _ = shutdown_and_reap_guardian(child, tree, lease);
        }
        // Keep the disk backstop until guardian exit/reap has been observed.
        drop(lease);
    }
}

/// Spawn a long-lived mpv behind the same-binary guardian.
pub(crate) fn spawn(program: &str, mut args: Vec<String>, detached: bool) -> Result<GuardedSpawn> {
    let identity_marker = new_identity_marker()?;
    // mpv accepts arbitrary script options even if no matching script is installed. This marker
    // gives next-start recovery a unique command-line identity for audio and IPC-less overlays.
    args.push(format!(
        "--script-opts-append=yututui-lifeline={identity_marker}"
    ));
    let request = GuardianRequest {
        program: program.to_owned(),
        args,
        detached,
        mode: GuardianMode::LongLived,
        windows_inner_job: None,
    };
    let mut pending = start_guardian(request, true)?;
    let response = pending.response(STARTUP_TIMEOUT)?;
    let mpv_pid = match response {
        GuardianResponse::Ready { mpv_pid } => mpv_pid,
        GuardianResponse::Error { message } => bail!("mpv guardian rejected spawn: {message}"),
        GuardianResponse::Probe { .. } => bail!("mpv guardian returned an invalid spawn reply"),
    };

    // The guardian was published before the heartbeat writer sent this request. Atomically replace
    // that temporary `(guardian, guardian)` slot with `(mpv, guardian)`; shutdown racing either
    // side of this CAS wins fail-closed without a spawn-before-registry interval.
    pending
        .lease
        .as_mut()
        .context("guardian lease ownership missing")?
        .registration
        .as_mut()
        .context("guardian process registration missing")?
        .publish_mpv_pid(mpv_pid)
        .context("publish guarded mpv process identity")?;
    pending
        .lease
        .as_mut()
        .context("guardian lease ownership missing")?
        .disk_registration = super::lifetime::register_guarded_lifeline(mpv_pid, &identity_marker)
        .context("persist mpv lifeline registry")?;

    Ok(GuardedSpawn {
        child_tree: Some(
            pending
                .child_tree
                .take()
                .context("guardian tree ownership missing")?,
        ),
        child: Some(
            pending
                .child
                .take()
                .context("guardian process ownership missing")?,
        ),
        lease: Some(
            pending
                .lease
                .take()
                .context("guardian lease ownership missing")?,
        ),
        mpv_pid,
    })
}

fn new_identity_marker() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate mpv lifeline identity")?;
    let mut marker = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut marker, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(marker)
}

/// Run a bounded mpv probe without ever launching an unguarded mpv process.
pub(crate) fn probe(
    program: &str,
    args: Vec<String>,
    timeout: Duration,
    stdout_max: usize,
) -> Result<ProbeOutput> {
    if timeout > MAX_PROBE_TIMEOUT {
        bail!("mpv probe timeout exceeds guardian limit");
    }
    if stdout_max > MAX_PROBE_OUTPUT {
        bail!("mpv probe output limit exceeds guardian limit");
    }
    let request = GuardianRequest {
        program: program.to_owned(),
        args,
        detached: false,
        mode: GuardianMode::Probe {
            timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
            stdout_max,
        },
        windows_inner_job: None,
    };
    let mut pending = start_guardian(request, false)?;
    let response = pending.response(timeout.saturating_add(Duration::from_secs(3)))?;
    let result = match response {
        GuardianResponse::Probe { success, stdout } => ProbeOutput { success, stdout },
        GuardianResponse::Error { message } => bail!("mpv guardian probe failed: {message}"),
        GuardianResponse::Ready { .. } => bail!("mpv guardian returned an invalid probe reply"),
    };
    pending.finish_after_response();
    Ok(result)
}

struct PendingGuardian {
    child_tree: Option<ChildTreeGuard>,
    child: Option<Child>,
    lease: Option<GuardianLease>,
    stdout: Option<std::process::ChildStdout>,
}

impl PendingGuardian {
    fn response(&mut self, timeout: Duration) -> Result<GuardianResponse> {
        let stdout = self
            .stdout
            .take()
            .context("mpv guardian response pipe unavailable")?;
        let (sent, received) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("ytt-mpv-guardian-reply".to_owned())
            .spawn(move || {
                let mut line = Vec::new();
                let mut reader = std::io::BufReader::new(stdout).take((RESPONSE_MAX + 1) as u64);
                let result = reader
                    .read_until(b'\n', &mut line)
                    .map_err(anyhow::Error::from)
                    .and_then(|_| {
                        if line.len() > RESPONSE_MAX || !line.ends_with(b"\n") {
                            bail!("mpv guardian response exceeded its bound or was truncated");
                        }
                        serde_json::from_slice(&line).context("decode mpv guardian response")
                    });
                let _ = sent.send(result);
            })
            .context("start mpv guardian response reader")?;
        match received.recv_timeout(timeout) {
            Ok(response) => response,
            Err(RecvTimeoutError::Timeout) => {
                self.abort();
                bail!("mpv guardian response timed out")
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.abort();
                bail!("mpv guardian response reader stopped")
            }
        }
    }

    fn finish_after_response(&mut self) {
        let mut lease = self.lease.take();
        if let (Some(lease), Some(child), Some(tree)) =
            (lease.as_mut(), self.child.take(), self.child_tree.as_mut())
        {
            let _ = shutdown_and_reap_guardian(child, tree, lease);
        }
        drop(lease);
        self.child = None;
        self.child_tree = None;
    }

    fn abort(&mut self) {
        let mut lease = self.lease.take();
        if let (Some(lease), Some(child), Some(tree)) =
            (lease.as_mut(), self.child.take(), self.child_tree.as_mut())
        {
            let _ = shutdown_and_reap_guardian(child, tree, lease);
        }
        drop(lease);
        self.child = None;
        self.child_tree = None;
    }
}

impl Drop for PendingGuardian {
    fn drop(&mut self) {
        self.abort();
    }
}

fn start_guardian(mut request: GuardianRequest, long_lived: bool) -> Result<PendingGuardian> {
    let exe = std::env::current_exe().context("locate ytt executable for mpv guardian")?;
    let exe = exe
        .to_str()
        .context("ytt executable path is not valid UTF-8")?;
    let mut command = process::std_command(exe, ProcessProfile::Media);
    command
        .arg("__mpv-guardian")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = command.spawn().context("start mpv guardian")?;
    let mut protected = ProtectedChild::new(child, ProcessProfile::Media);
    request.windows_inner_job = protected
        .tree()
        .guardian_token()
        .context("arm mpv guardian process tree")?;

    let writer = protected
        .child_mut()
        .stdin
        .take()
        .context("mpv guardian request pipe missing")?;
    let request_frame = encode_request(&request)?;
    // Publish the still-blocked guardian before either heartbeat owner sends the request. The
    // independent terminal watchdog can therefore kill long-lived startup and bounded probes with
    // no spawn-before-registration window. Long-lived Ready later upgrades the low word atomically.
    let guardian_pid = protected.id();
    let registration = super::lifetime::register_live_mpv(guardian_pid, guardian_pid)
        .context("register blocked mpv guardian")?;
    // Start before waiting for either Ready or a completed probe. The guardian begins its timeout
    // as soon as mpv exists, and a slow executable must not look like owner death.
    let lease_result = if long_lived {
        GuardianLease::new_runtime(writer, request_frame, registration)
    } else {
        GuardianLease::new_probe(writer, request_frame, registration)
    };
    let lease = lease_result?;
    let stdout = protected.child_mut().stdout.take();
    let (child_tree, child) = protected.into_parts();
    let pending = PendingGuardian {
        child_tree: Some(child_tree),
        child: Some(child),
        lease: Some(lease),
        stdout,
    };
    Ok(pending)
}

fn runtime_heartbeat_writer(
    mut writer: impl Write,
    request_frame: Vec<u8>,
    stopped: Receiver<()>,
    pulse: Arc<AtomicU64>,
    stale_timeout: Duration,
) {
    if writer
        .write_all(&request_frame)
        .and_then(|_| writer.write_all(&[0xA5]))
        .and_then(|_| writer.flush())
        .is_err()
    {
        return;
    }
    let mut observed = pulse.load(Ordering::Acquire);
    let mut last_runtime_pulse = Instant::now();
    loop {
        match stopped.recv_timeout(RUNTIME_PULSE_POLL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let current = pulse.load(Ordering::Acquire);
        if current != observed {
            observed = current;
            last_runtime_pulse = Instant::now();
            if writer
                .write_all(&[0xA5])
                .and_then(|_| writer.flush())
                .is_err()
            {
                return;
            }
        } else if last_runtime_pulse.elapsed() >= stale_timeout {
            // The OS writer remains schedulable when Tokio is frozen, but deliberately closes the
            // pipe instead of masking that freeze from the guardian.
            return;
        }
    }
}

fn heartbeat_writer(
    mut writer: std::process::ChildStdin,
    request_frame: Vec<u8>,
    stopped: Receiver<()>,
) {
    if writer
        .write_all(&request_frame)
        .and_then(|_| writer.flush())
        .is_err()
    {
        return;
    }
    loop {
        if writer
            .write_all(&[0xA5])
            .and_then(|_| writer.flush())
            .is_err()
        {
            return;
        }
        match stopped.recv_timeout(HEARTBEAT_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
fn write_request(writer: &mut impl Write, request: &GuardianRequest) -> Result<()> {
    let frame = encode_request(request)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

fn encode_request(request: &GuardianRequest) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(request).context("encode mpv guardian request")?;
    if bytes.len() > REQUEST_MAX {
        bail!("mpv guardian request is too large");
    }
    let len: u32 = bytes
        .len()
        .try_into()
        .context("mpv guardian request length overflow")?;
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&bytes);
    Ok(frame)
}

fn read_request(reader: &mut impl Read) -> Result<GuardianRequest> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > REQUEST_MAX {
        bail!("mpv guardian request exceeds its bound");
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    let request: GuardianRequest =
        serde_json::from_slice(&bytes).context("decode mpv guardian request")?;
    if request.program.is_empty() || request.args.len() > 4096 {
        bail!("invalid mpv guardian spawn request");
    }
    #[cfg(windows)]
    if request.windows_inner_job.is_none() {
        bail!("mpv guardian has no inner Job Object");
    }
    if let GuardianMode::Probe {
        timeout_ms,
        stdout_max,
        ..
    } = &request.mode
        && (*timeout_ms == 0
            || Duration::from_millis(*timeout_ms) > MAX_PROBE_TIMEOUT
            || *stdout_max > MAX_PROBE_OUTPUT)
    {
        bail!("mpv guardian probe limits are invalid");
    }
    Ok(request)
}

fn write_response(response: &GuardianResponse) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_response_to(&mut stdout, response)?;
    Ok(())
}

fn write_response_to(writer: &mut impl Write, response: &GuardianResponse) -> Result<()> {
    serde_json::to_writer(&mut *writer, response).context("encode mpv guardian response")?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(unix)]
fn install_native_lease_arg(
    args: &mut Vec<String>,
    mode: &GuardianMode,
    lease_arg: String,
) -> Result<()> {
    if args.iter().any(|arg| arg == "--") {
        bail!("mpv lifetime protection unavailable: bare -- can bypass the native IPC lease");
    }
    match mode {
        GuardianMode::LongLived => {
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--version" | "-V" | "--help" | "-h"))
            {
                bail!(
                    "mpv lifetime protection unavailable: early-exit option cannot be used for playback"
                );
            }
            // Last-option-wins prevents user/config arguments from replacing the safety client.
            args.push(lease_arg);
        }
        GuardianMode::Probe { .. } => {
            // mpv short-circuits option parsing at --version. Insert immediately before it so an
            // old build fails the capability probe instead of silently ignoring the lease.
            let index = args
                .iter()
                .position(|arg| matches!(arg.as_str(), "--version" | "-V" | "--help" | "-h"))
                .unwrap_or(args.len());
            args.insert(index, lease_arg);
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" fn guardian_sigterm_handler(_signal: libc::c_int) {
    // AtomicBool is lock-free on supported Unix targets. The handler deliberately performs no
    // allocation, logging, locking, or process management; the normal supervision loop reaps mpv.
    GUARDIAN_TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_guardian_sigterm_handler() -> std::io::Result<()> {
    GUARDIAN_TERMINATION_REQUESTED.store(false, Ordering::SeqCst);
    // SAFETY: the zeroed action is fully initialized below before registration. The callback has
    // C ABI and static lifetime, and its body is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = guardian_sigterm_handler as *const () as usize;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn guardian_termination_requested() -> bool {
    GUARDIAN_TERMINATION_REQUESTED.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn guardian_termination_requested() -> bool {
    false
}

fn ensure_guardian_may_spawn() -> Result<()> {
    if guardian_termination_requested() {
        bail!("mpv guardian termination was requested before spawn");
    }
    Ok(())
}

/// Hidden same-binary command entry. `main` must dispatch this before runtime, persistence, or TUI
/// initialization and exit with the returned status.
pub fn run_cli() -> i32 {
    #[cfg(unix)]
    if let Err(error) = install_guardian_sigterm_handler() {
        let _ = write_response(&GuardianResponse::Error {
            message: format!("install mpv guardian SIGTERM handler: {error}"),
        });
        return 1;
    }
    match run_guardian(std::io::stdin()) {
        Ok(code) => code,
        Err(error) => {
            let _ = write_response(&GuardianResponse::Error {
                message: error.to_string(),
            });
            1
        }
    }
}

fn run_guardian(mut input: std::io::Stdin) -> Result<i32> {
    let request = read_request(&mut input)?;
    // The request alone is not permission to spawn. The owner must have successfully installed
    // its runtime/probe heartbeat writer and deliver this explicit first pulse.
    read_startup_heartbeat(&mut input)?;
    let (lease_tx, lease_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("ytt-mpv-owner-lease".to_owned())
        .spawn(move || read_owner_lease(input, lease_tx))
        .context("start mpv owner-lease reader")?;
    ensure_guardian_may_spawn()?;

    let mut command = process::std_command(&request.program, ProcessProfile::Media);
    let guarded_args = request.args.clone();
    #[cfg(unix)]
    let mut guarded_args = guarded_args;
    command.stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    if request.detached {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }

    #[cfg(unix)]
    let (mut mpv_lease, inherited_lease) = {
        use std::os::fd::AsRawFd;
        let (guardian, inherited) =
            std::os::unix::net::UnixStream::pair().context("create mpv native IPC lease")?;
        let fd = inherited.as_raw_fd();
        process::inherit_fd_in_child(&mut command, fd);
        install_native_lease_arg(
            &mut guarded_args,
            &request.mode,
            format!("--input-ipc-client=fd://{fd}"),
        )?;
        (NativeMpvLease::new(guardian)?, inherited)
    };
    #[cfg(unix)]
    process::configure_parent_death_signal(&mut command);
    command.args(&guarded_args);
    ensure_guardian_may_spawn()?;

    match request.mode {
        GuardianMode::LongLived => {
            command.stdout(Stdio::null());
            let child = command.spawn().context("spawn protected mpv")?;
            #[cfg(unix)]
            drop(inherited_lease);
            let mut child = ProtectedChild::new(child, ProcessProfile::Media);
            if guardian_termination_requested() {
                child
                    .terminate_and_wait()
                    .context("reap mpv after pre-spawn guardian termination request")?;
                return Ok(0);
            }
            write_response(&GuardianResponse::Ready {
                mpv_pid: child.id(),
            })?;
            let result = supervise(&mut child, &lease_rx, None);
            #[cfg(unix)]
            mpv_lease.close();
            debug_assert!(child.is_reaped());
            finish_guardian(result, request.windows_inner_job)
        }
        GuardianMode::Probe {
            timeout_ms,
            stdout_max,
        } => {
            command.stdout(Stdio::piped());
            let child = command.spawn().context("spawn protected mpv probe")?;
            #[cfg(unix)]
            drop(inherited_lease);
            let mut child = ProtectedChild::new(child, ProcessProfile::Media);
            if guardian_termination_requested() {
                child
                    .terminate_and_wait()
                    .context("reap mpv probe after pre-spawn guardian termination request")?;
                return Ok(0);
            }
            let stdout = child
                .take_stdout()
                .context("mpv probe output pipe missing")?;
            let output = std::thread::Builder::new()
                .name("ytt-mpv-probe-output".to_owned())
                .spawn(move || {
                    let mut bytes = Vec::new();
                    stdout
                        .take((stdout_max.saturating_add(1)) as u64)
                        .read_to_end(&mut bytes)
                        .map(|_| bytes)
                })
                .context("start protected mpv probe output reader")?;
            let timeout = Duration::from_millis(timeout_ms).min(MAX_PROBE_TIMEOUT);
            let result = supervise(&mut child, &lease_rx, Some(timeout));
            let stdout = join_probe_output(output)?;
            if stdout.len() > stdout_max {
                bail!("mpv probe output exceeded its bound");
            }
            let success = result?.unwrap_or(false);
            #[cfg(unix)]
            mpv_lease.close();
            debug_assert!(child.is_reaped());
            write_response(&GuardianResponse::Probe { success, stdout })?;
            Ok(0)
        }
    }
}

fn read_startup_heartbeat(reader: &mut impl Read) -> Result<()> {
    let mut first_heartbeat = [0u8; 1];
    reader
        .read_exact(&mut first_heartbeat)
        .context("mpv guardian owner did not send the startup heartbeat")?;
    if first_heartbeat != [0xA5] {
        bail!("mpv guardian owner sent an invalid startup heartbeat");
    }
    Ok(())
}

fn read_owner_lease(mut input: std::io::Stdin, events: mpsc::Sender<LeaseEvent>) {
    let mut buf = [0u8; 64];
    loop {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = events.send(LeaseEvent::Closed);
                return;
            }
            Ok(_) => {
                if events.send(LeaseEvent::Heartbeat).is_err() {
                    return;
                }
            }
        }
    }
}

fn supervise(
    child: &mut ProtectedChild,
    lease: &Receiver<LeaseEvent>,
    timeout: Option<Duration>,
) -> Result<Option<bool>> {
    supervise_with_termination(child, lease, timeout, guardian_termination_requested)
}

fn supervise_with_termination(
    child: &mut ProtectedChild,
    lease: &Receiver<LeaseEvent>,
    timeout: Option<Duration>,
    termination_requested: impl Fn() -> bool,
) -> Result<Option<bool>> {
    let result = supervise_inner(child, lease, timeout, termination_requested);
    match result {
        Ok(result) => {
            debug_assert!(child.is_reaped());
            Ok(result)
        }
        Err(error) => {
            if !child.is_reaped()
                && let Err(cleanup_error) = child.terminate_and_wait()
            {
                return Err(anyhow::anyhow!(
                    "{error:#}; protected mpv cleanup also failed: {cleanup_error}"
                ));
            }
            Err(error)
        }
    }
}

fn supervise_inner(
    child: &mut ProtectedChild,
    lease: &Receiver<LeaseEvent>,
    timeout: Option<Duration>,
    termination_requested: impl Fn() -> bool,
) -> Result<Option<bool>> {
    let started = Instant::now();
    let mut heartbeat = Instant::now();
    loop {
        if termination_requested() {
            child
                .terminate_and_wait()
                .context("reap protected mpv after guardian SIGTERM request")?;
            return Ok(None);
        }
        #[cfg(any(unix, windows))]
        if let Some(success) = process::child_exit_without_reap(child.child())
            .context("poll protected mpv without reaping")?
        {
            // Kill any remaining member while the direct child is still an unreaped group leader,
            // then reap it here. Leaving the zombie for PID 1 is not safe in minimal containers
            // whose init process does not reliably reap adopted children. The owner registry sends
            // only cooperative SIGTERM to the still-owned guardian, never to this mpv pid.
            let status = child.reap_exited().context("reap protected mpv")?;
            debug_assert_eq!(status.success(), success);
            return Ok(Some(status.success()));
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            child
                .terminate_and_wait()
                .context("reap protected mpv after probe timeout")?;
            return Ok(None);
        }
        if heartbeat.elapsed() >= HEARTBEAT_TIMEOUT {
            child
                .terminate_and_wait()
                .context("reap protected mpv after owner heartbeat timeout")?;
            return Ok(None);
        }
        match lease.recv_timeout(CHILD_POLL) {
            Ok(LeaseEvent::Heartbeat) => heartbeat = Instant::now(),
            Ok(LeaseEvent::Closed) | Err(RecvTimeoutError::Disconnected) => {
                child
                    .terminate_and_wait()
                    .context("reap protected mpv after owner lease closed")?;
                return Ok(None);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn finish_guardian(result: Result<Option<bool>>, windows_inner_job: Option<u64>) -> Result<i32> {
    if let Err(error) = result {
        #[cfg(windows)]
        if let Some(job) = windows_inner_job {
            // This normally terminates the guardian itself and never returns. If Windows rejects
            // it, the process-tree kill already ran and returning closes the last inner handle.
            let _ = process::terminate_inherited_job(job);
        }
        #[cfg(not(windows))]
        let _ = windows_inner_job;
        return Err(error);
    }
    Ok(0)
}

fn join_probe_output(output: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    let deadline = Instant::now() + PROBE_OUTPUT_JOIN_TIMEOUT;
    while !output.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !output.is_finished() {
        // A descendant that escaped its process group may retain stdout indefinitely. Detaching
        // the reader is bounded here: this guardian process is about to exit, which tears the
        // thread and its descriptor down even if the escaped writer never closes its copy.
        drop(output);
        bail!("mpv probe output remained open after the protected process tree exited");
    }
    output
        .join()
        .map_err(|_| anyhow::anyhow!("mpv probe output reader panicked"))?
        .context("read protected mpv probe output")
}

pub(crate) fn wait_for_guardian_exit_without_reap(
    child: &Child,
    timeout: Duration,
) -> std::io::Result<Option<bool>> {
    let deadline = Instant::now() + timeout;
    loop {
        match process::child_exit_without_reap(child)? {
            Some(clean) => return Ok(Some(clean)),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => return Ok(None),
        }
    }
}

/// Unix owner teardown never kills or abandons the guardian: close heartbeat, request cooperative
/// SIGTERM if needed, then wait until the guardian has terminated and reaped mpv. An
/// uninterruptible kernel wait intentionally keeps ytt alive as the guardian's parent rather than
/// delegating either process to an unreliable container PID 1.
#[cfg(unix)]
pub(crate) fn shutdown_and_reap_guardian(
    mut child: Child,
    child_tree: &mut ChildTreeGuard,
    lease: &mut GuardianLease,
) -> Option<std::process::ExitStatus> {
    lease.request_shutdown();
    if let Ok(Some(clean)) = wait_for_guardian_exit_without_reap(&child, Duration::from_millis(500))
    {
        lease.disarm_after_guardian_exit(clean);
        // The guardian pid/process object is still retained, so this cannot target a reused group.
        child_tree.terminate();
        return child.wait().ok();
    }

    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        lease.disarm_before_unobserved_wait();
        child_tree.release_without_termination();
        return child.wait().ok();
    };
    process::request_process_termination(pid);
    loop {
        match process::child_exit_without_reap(&child) {
            Ok(Some(clean)) => {
                lease.disarm_after_guardian_exit(clean);
                child_tree.terminate();
                return child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::warn!(%error, pid = child.id(), "non-reaping guardian wait failed");
                // Remove every raw pid/group hook before wait releases the process object. The
                // cooperative request and closed heartbeat are already delivered, and this
                // synchronous wait keeps the guardian available to reap mpv for as long as needed.
                lease.disarm_before_unobserved_wait();
                child_tree.release_without_termination();
                return child.wait().ok();
            }
        }
    }
}

/// Windows keeps the bounded hard fallback: the parent-only outer and guardian-only inner Jobs
/// preserve whole-tree ownership even if the guardian itself must be terminated.
#[cfg(windows)]
pub(crate) fn shutdown_and_reap_guardian(
    mut child: Child,
    child_tree: &mut ChildTreeGuard,
    lease: &mut GuardianLease,
) -> Option<std::process::ExitStatus> {
    lease.request_shutdown();
    if let Ok(Some(clean)) = wait_for_guardian_exit_without_reap(&child, Duration::from_millis(500))
    {
        lease.disarm_after_guardian_exit(clean);
        child_tree.terminate();
        return child.wait().ok();
    }

    lease.hard_kill_preserving_disk();
    child_tree.terminate();
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match process::child_exit_without_reap(&child) {
            Ok(Some(_)) => return child.wait().ok(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => break,
        }
    }

    let pid = child.id();
    tracing::warn!(pid, "mpv guardian exceeded its hard reap deadline");
    let reaper_child = Arc::new(std::sync::Mutex::new(Some(child)));
    let thread_child = Arc::clone(&reaper_child);
    if let Err(error) = std::thread::Builder::new()
        .name("ytt-mpv-guardian-reaper".to_owned())
        .spawn(move || {
            let mut child = thread_child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(child) = child.as_mut()
                && let Err(error) = child.wait()
            {
                tracing::warn!(%error, pid, "background mpv guardian reap failed");
            }
        })
    {
        // Thread creation can fail under resource exhaustion. Recover ownership from the closure
        // container and reap synchronously instead of dropping `Child` and leaving a zombie behind.
        tracing::warn!(%error, pid, "failed to start mpv guardian background reaper");
        let mut child = reaper_child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(child) = child.as_mut()
            && let Err(error) = child.wait()
        {
            tracing::warn!(%error, pid, "synchronous mpv guardian reap failed");
        }
    }
    None
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn shutdown_and_reap_guardian(
    mut child: Child,
    child_tree: &mut ChildTreeGuard,
    lease: &mut GuardianLease,
) -> Option<std::process::ExitStatus> {
    lease.request_shutdown();
    child_tree.terminate();
    child.wait().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame_is_bounded_and_round_trips() {
        let request = GuardianRequest {
            program: "mpv".to_owned(),
            args: vec!["--version".to_owned()],
            detached: false,
            mode: GuardianMode::Probe {
                timeout_ms: 250,
                stdout_max: 1024,
            },
            windows_inner_job: None,
        };
        let mut frame = Vec::new();
        write_request(&mut frame, &request).unwrap();
        let decoded = read_request(&mut frame.as_slice()).unwrap();
        assert_eq!(decoded.program, "mpv");
        assert_eq!(decoded.args, ["--version"]);
        assert!(matches!(decoded.mode, GuardianMode::Probe { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn native_lease_precedes_probe_short_circuit_and_rejects_option_terminator() {
        let mode = GuardianMode::Probe {
            timeout_ms: 250,
            stdout_max: 1024,
        };
        let mut args = vec!["--no-config".to_owned(), "--version".to_owned()];
        install_native_lease_arg(&mut args, &mode, "--input-ipc-client=fd://9".to_owned()).unwrap();
        assert_eq!(
            args,
            ["--no-config", "--input-ipc-client=fd://9", "--version"]
        );

        let mut bypass = vec!["--".to_owned(), "--version".to_owned()];
        assert!(
            install_native_lease_arg(
                &mut bypass,
                &GuardianMode::LongLived,
                "--input-ipc-client=fd://9".to_owned(),
            )
            .is_err()
        );

        let mut long_lived = vec!["--input-ipc-client=fd://999".to_owned()];
        install_native_lease_arg(
            &mut long_lived,
            &GuardianMode::LongLived,
            "--input-ipc-client=fd://9".to_owned(),
        )
        .unwrap();
        assert_eq!(long_lived.last().unwrap(), "--input-ipc-client=fd://9");
    }

    #[test]
    fn guardian_requires_exact_go_heartbeat_before_spawn_phase() {
        assert!(read_startup_heartbeat(&mut [].as_slice()).is_err());
        assert!(read_startup_heartbeat(&mut [0x00].as_slice()).is_err());
        assert!(read_startup_heartbeat(&mut [0xA5].as_slice()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_heartbeat_writer_closes_when_the_async_runtime_stops_pulsing() {
        let (writer, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (stop, stopped) = mpsc::channel();
        let pulse = Arc::new(AtomicU64::new(1));
        let worker = std::thread::spawn(move || {
            runtime_heartbeat_writer(
                writer,
                vec![1, 2, 3],
                stopped,
                pulse,
                Duration::from_millis(25),
            )
        });

        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        worker.join().unwrap();
        drop(stop);
        assert_eq!(bytes, [1, 2, 3, 0xA5]);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn owner_shutdown_prefers_pipe_eof_then_cooperative_sigterm() {
        let _pid_guard = super::super::lifetime::lock_mpv_pid_for_test().await;
        for (script, expect_success) in [("cat >/dev/null", true), ("sleep 30", false)] {
            let mut command = process::std_command("sh", ProcessProfile::Media);
            command
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().unwrap();
            let mut tree = ChildTreeGuard::for_std(&child, ProcessProfile::Media);
            let registration =
                super::super::lifetime::register_live_mpv(child.id(), child.id()).unwrap();
            let writer = child.stdin.take().unwrap();
            let mut lease = GuardianLease::new_probe(writer, Vec::new(), registration).unwrap();

            let started = Instant::now();
            let status = shutdown_and_reap_guardian(child, &mut tree, &mut lease);
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(
                status.is_some_and(|status| status.success()),
                expect_success
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn closed_owner_lease_terminates_the_whole_media_group() {
        let mut command = process::std_command("sh", ProcessProfile::Media);
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let mut child = ProtectedChild::new(child, ProcessProfile::Media);
        let (tx, rx) = mpsc::channel();
        tx.send(LeaseEvent::Closed).unwrap();
        let result = supervise(&mut child, &rx, None);
        assert_eq!(result.unwrap(), None);
        assert!(child.is_reaped());
        assert!(!process::process_exists_for_test(pid));
    }

    #[cfg(unix)]
    #[test]
    fn natural_mpv_exit_is_reaped_before_guardian_completion() {
        let mut command = process::std_command("sh", ProcessProfile::Media);
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let mut child = ProtectedChild::new(child, ProcessProfile::Media);
        let (_lease_owner, lease) = mpsc::channel();

        assert_eq!(supervise(&mut child, &lease, None).unwrap(), Some(true));
        assert!(child.is_reaped());
        assert!(
            !process::process_exists_for_test(pid),
            "the guardian must never leave its exited mpv for container PID 1 to reap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cooperative_guardian_termination_kills_and_reaps_mpv() {
        let mut command = process::std_command("sh", ProcessProfile::Media);
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let mut child = ProtectedChild::new(child, ProcessProfile::Media);
        let (_lease_owner, lease) = mpsc::channel();

        let result = supervise_with_termination(&mut child, &lease, None, || true).unwrap();
        assert_eq!(result, None);
        assert!(child.is_reaped());
        assert!(
            !process::process_exists_for_test(pid),
            "cooperative SIGTERM handling must retain the guardian long enough to reap mpv"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_ready_response_still_terminates_and_reaps_spawned_mpv() {
        struct BrokenWriter;

        impl Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "owner response pipe closed",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pid = 0;
        let result: Result<()> = (|| {
            let mut command = process::std_command("sh", ProcessProfile::Media);
            command
                .args(["-c", "sleep 30 & wait"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let child = command.spawn()?;
            pid = child.id();
            let _child = ProtectedChild::new(child, ProcessProfile::Media);
            write_response_to(&mut BrokenWriter, &GuardianResponse::Ready { mpv_pid: pid })?;
            Ok(())
        })();

        assert!(result.is_err());
        assert_ne!(pid, 0);
        assert!(
            !process::process_exists_for_test(libc::pid_t::try_from(pid).unwrap()),
            "Ready EPIPE cleanup must reap mpv instead of delegating it to PID 1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_mpv_lease_drains_more_than_a_socket_buffer() {
        let (guardian, mut mpv) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut lease = NativeMpvLease::new(guardian).unwrap();
        let writer = std::thread::spawn(move || {
            let block = [b'x'; 16 * 1024];
            for _ in 0..256 {
                mpv.write_all(&block).unwrap();
            }
        });
        writer.join().expect("IPC drain must prevent writer stall");
        lease.close();
    }
}
