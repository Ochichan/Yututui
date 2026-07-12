//! No-orphan process lifetime: mpv must die with the app, on every exit path.
//!
//! Layered guarantees (see the plan's "Process lifetime" section):
//!   1. tokio `kill_on_drop` + the [`super::Mpv`] `Drop` guard      — normal quit.
//!   2. Unix signal handlers (SIGINT/SIGTERM/SIGHUP)                — `kill`, terminal/SSH close.
//!   3. Panic hook (covers `panic = "abort"`, where Drop won't run) — crashes.
//!   4. PID registry reaped on next startup                        — uncatchable SIGKILL/power loss.
//!
//! On Windows the Job Object (M9) supersedes (2)/(3) by having the kernel terminate
//! mpv when our process dies for any reason; the registry stays as a universal backstop.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

/// The live mpv pid, or 0 if none. Read from the panic hook and signal handler, which
/// must be allocation-free and async-signal-safe — an atomic is exactly that.
static MPV_PID: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
static MPV_PID_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn lock_mpv_pid_for_test() -> tokio::sync::MutexGuard<'static, ()> {
    MPV_PID_TEST_LOCK.lock().await
}

/// Monotonic, out-of-band process shutdown signal.
///
/// Owner event queues are deliberately bounded and can be saturated at exactly the moment an
/// external signal arrives. This latch therefore does not share their delivery path: signal
/// handlers set the atomic first, then wake the owner loop through a watch channel. The atomic
/// makes checks cheap and monotonic; subscribing before the second check in [`Self::wait`]
/// makes the wait lost-wakeup safe.
#[derive(Clone)]
pub struct ShutdownLatch {
    inner: Arc<ShutdownLatchInner>,
}

struct ShutdownLatchInner {
    triggered: AtomicBool,
    changed: tokio::sync::watch::Sender<bool>,
}

impl ShutdownLatch {
    pub fn new() -> Self {
        let (changed, _rx) = tokio::sync::watch::channel(false);
        Self {
            inner: Arc::new(ShutdownLatchInner {
                triggered: AtomicBool::new(false),
                changed,
            }),
        }
    }

    /// Set the latch once and wake every current or future waiter.
    pub fn trigger(&self) {
        if !self.inner.triggered.swap(true, Ordering::AcqRel) {
            self.inner.changed.send_replace(true);
        }
    }

    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::Acquire)
    }

    /// Wait until shutdown is requested without losing a trigger that races registration.
    pub async fn wait(&self) {
        if self.is_triggered() {
            return;
        }
        let mut changed = self.inner.changed.subscribe();
        loop {
            if self.is_triggered() {
                return;
            }
            // The sender is owned by the same Arc as this receiver, so closure is impossible
            // while the wait is live. Treat it as shutdown defensively if that invariant ever
            // changes.
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for ShutdownLatch {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-lifetime events emitted by signal handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEvent {
    Quit,
}

fn request_signal_shutdown<F>(shutdown: &ShutdownLatch, emit: &F)
where
    F: Fn(SignalEvent),
{
    // Ordering is intentional: the owner must suppress recovery before killing mpv can cause
    // its IPC actor to enqueue TransportClosed into an already-saturated owner lane.
    shutdown.trigger();
    kill_mpv_now();
    emit(SignalEvent::Quit);
}

/// Record the spawned mpv pid so the panic hook / signal handler can reach it.
pub fn set_mpv_pid(pid: u32) {
    MPV_PID.store(pid, Ordering::SeqCst);
}

/// Atomically take the recorded pid (leaving 0), so we kill it at most once.
fn take_mpv_pid() -> Option<u32> {
    match MPV_PID.swap(0, Ordering::SeqCst) {
        0 => None,
        pid => Some(pid),
    }
}

/// Kill the recorded mpv immediately, if any. Safe to call repeatedly.
pub fn kill_mpv_now() {
    if let Some(pid) = take_mpv_pid() {
        kill_pid(pid);
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    // The shared process abstraction owns the only unsafe Unix kill primitive. It targets mpv's
    // process group (including ytdl_hook helpers) and falls back to the direct pid for lifeline
    // records written by versions that launched Media children without group isolation.
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        crate::util::process::terminate_process_group(pid);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    // Belt-and-suspenders next to the Job Object (M9): ask the OS to terminate it.
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[target]), true);
    if let Some(proc_) = sys.process(target) {
        proc_.kill();
    }
}

/// Wrap the current panic hook so a panic (including `panic = "abort"`, where `Drop`
/// never runs) still kills mpv before the inherited hook restores the terminal.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        kill_mpv_now();
        previous(info);
    }));
}

/// Spawn a task that waits for any termination signal, kills mpv, and asks the main
/// loop to quit. Keyboard Ctrl+C is handled as a key event (raw mode swallows SIGINT),
/// so these cover external `kill`s and terminal/SSH disconnects (SIGHUP).
#[cfg(unix)]
pub fn spawn_signal_handlers<F>(
    shutdown: ShutdownLatch,
    emit: F,
) -> crate::util::background_task::BackgroundTask
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
{
    use tokio::signal::unix::{SignalKind, signal};

    crate::util::background_task::BackgroundTask::spawn("termination signal handlers", async move {
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGHUP handler");
                return;
            }
        };
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGTERM handler");
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGINT handler");
                return;
            }
        };

        tokio::select! {
            _ = hup.recv() => tracing::info!("received SIGHUP"),
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
        }

        request_signal_shutdown(&shutdown, &emit);
    })
}

#[cfg(windows)]
pub fn spawn_signal_handlers<F>(
    shutdown: ShutdownLatch,
    emit: F,
) -> crate::util::background_task::BackgroundTask
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
{
    // Console Ctrl+C; CTRL_CLOSE/logoff/shutdown are owned by the Job Object below.
    crate::util::background_task::BackgroundTask::spawn("termination signal handlers", async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            request_signal_shutdown(&shutdown, &emit);
        }
    })
}

/// Windows: register a console control handler that kills mpv on the close button,
/// logoff, and shutdown. [`crate::util::process_guard::ChildTreeGuard`] already owns the
/// kill-on-close Job Object; this makes teardown prompt while the OS allows cleanup.
#[cfg(windows)]
pub fn install_console_ctrl_handler() {
    use windows_sys::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
    };

    // `windows-sys` models Win32 `BOOL` as a plain `i32` (1 = TRUE, 0 = FALSE).
    /// # Safety
    /// Called only by the Windows console subsystem with the documented control-event
    /// code; the handler does not dereference raw pointers or retain OS-owned state.
    // SAFETY: registered only through SetConsoleCtrlHandler with the required system
    // ABI; the body performs only atomic pid access and a best-effort kill.
    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        kill_mpv_now();
        match ctrl_type {
            // Close / logoff / shutdown: cleanup done; the OS still terminates us.
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => 1,
            // Ctrl+C / Ctrl+Break: let default handling proceed.
            _ => 0,
        }
    }

    // SAFETY: `handler` has the required system ABI and static lifetime; failure to
    // register is reported by the return value and only weakens prompt cleanup.
    unsafe {
        if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
            tracing::warn!("SetConsoleCtrlHandler failed");
        }
    }
}

/// On-disk record tying an mpv pid to the app instance that spawned it.
#[derive(Serialize, Deserialize)]
struct Lifeline {
    app_pid: u32,
    mpv_pid: u32,
    /// The unique IPC socket/pipe path passed to this mpv (`--input-ipc-server=<path>`). The
    /// reaper matches it against the candidate's command line so a reused pid belonging to an
    /// *unrelated* mpv (e.g. one the user launched directly) is never killed. `serde(default)`
    /// keeps records written by older builds loadable (they fall back to the name check).
    #[serde(default)]
    mpv_socket: String,
    /// Unix seconds when the record was written (i.e. at mpv spawn). A very old record is
    /// treated as stale and not acted on. `serde(default)` keeps old records loadable.
    #[serde(default)]
    written_at: u64,
}

fn registry_path(dir: &Path) -> std::path::PathBuf {
    dir.join("mpv-lifeline.json")
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record `{app_pid, mpv_pid, mpv_socket, written_at}` so a later run can reap a leaked mpv —
/// and only that mpv, verified by its unique IPC socket path.
pub fn register(dir: &Path, app_pid: u32, mpv_pid: u32, mpv_socket: &str) {
    let record = Lifeline {
        app_pid,
        mpv_pid,
        mpv_socket: mpv_socket.to_owned(),
        written_at: unix_now(),
    };
    let _ = safe_fs::write_private_atomic_json(&registry_path(dir), &record);
}

/// Reap a previous instance's mpv if that instance died without cleaning up (the only
/// path not covered by signals/Drop/panic/Job Object — e.g. SIGKILL or power loss).
pub fn reap_orphans(dir: &Path) {
    let path = registry_path(dir);
    let Ok(data) = safe_fs::read_to_string_no_symlink(&path) else {
        return;
    };
    let Ok(record) = serde_json::from_str::<Lifeline>(&data) else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    // A record older than this is stale: mpv respawns on every track change, so a live mpv's
    // record is recent, and nothing survives a reboot. Don't act on an ancient descriptor whose
    // pid has very likely been recycled by now.
    const LIFELINE_TTL_SECS: u64 = 7 * 24 * 3600;
    if record.written_at != 0 && unix_now().saturating_sub(record.written_at) > LIFELINE_TTL_SECS {
        let _ = std::fs::remove_file(&path);
        return;
    }

    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    // Refresh only the two pids we actually look up (the prior app + its mpv), not every
    // process on the system — this runs on the cold-start path before the first frame. The
    // two `process()` lookups below behave identically to a full `ProcessesToUpdate::All`
    // refresh; this mirrors `kill_pid` above, which already scopes its refresh the same way.
    sys.refresh_processes(
        ProcessesToUpdate::Some(&[Pid::from_u32(record.app_pid), Pid::from_u32(record.mpv_pid)]),
        true,
    );

    let app_alive = sys.process(Pid::from_u32(record.app_pid)).is_some();
    if !app_alive
        && let Some(proc_) = sys.process(Pid::from_u32(record.mpv_pid))
        // Guard against pid reuse: it must still look like mpv AND carry *our* unique IPC
        // socket path — an unrelated mpv the user launched won't have it (see identity check).
        && proc_.name().to_string_lossy().to_lowercase().contains("mpv")
        && mpv_identity_matches(proc_, &record)
    {
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(record.mpv_pid) {
            crate::util::process::terminate_process_group(pid);
        }
        #[cfg(not(unix))]
        proc_.kill();
        tracing::warn!(
            mpv_pid = record.mpv_pid,
            "reaped orphaned mpv from a prior run"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// Confirm a candidate really is *our* mpv before killing it, defeating pid reuse: an unrelated
/// mpv won't carry our unique `--input-ipc-server=<socket>` path in its command line.
fn mpv_identity_matches(proc_: &sysinfo::Process, record: &Lifeline) -> bool {
    let args: Vec<String> = proc_
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    mpv_identity_matches_args(&args, record)
}

fn mpv_identity_matches_args(args: &[String], record: &Lifeline) -> bool {
    if record.mpv_socket.is_empty() {
        // Legacy records had no socket identity; keep the historical name-check-only fallback.
        return true;
    }
    if args.is_empty() {
        // New records have a unique socket identity. If the OS will not expose argv, do not risk
        // killing an unrelated user-started mpv after PID reuse.
        return false;
    }
    args.iter()
        .any(|arg| arg.contains(record.mpv_socket.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-lifetime-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn mpv_pid_take_is_single_use_and_resets_to_empty() {
        let _pid_guard = lock_mpv_pid_for_test().await;
        let _ = take_mpv_pid();
        assert_eq!(take_mpv_pid(), None);

        set_mpv_pid(123_456);
        assert_eq!(take_mpv_pid(), Some(123_456));
        assert_eq!(take_mpv_pid(), None);
    }

    #[tokio::test]
    async fn shutdown_latch_wakes_independently_of_a_full_owner_queue() {
        let (owner_tx, _owner_rx) = tokio::sync::mpsc::channel(1);
        owner_tx.try_send(()).expect("fill owner queue");

        let latch = ShutdownLatch::new();
        let waiter = latch.clone();
        let waiting = tokio::spawn(async move {
            waiter.wait().await;
        });
        tokio::task::yield_now().await;

        latch.trigger();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("out-of-band latch must not wait for owner capacity")
            .expect("wait task must finish");
        assert!(latch.is_triggered());
        assert!(matches!(
            owner_tx.try_send(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(()))
        ));
    }

    #[tokio::test]
    async fn shutdown_latch_wait_is_lost_wakeup_safe_and_monotonic() {
        let latch = ShutdownLatch::new();
        let wait_created_before_trigger = latch.wait();

        // An async-fn body is not polled until awaited. Triggering here exercises the edge in
        // which registration has not happened yet; the atomic pre-check must still complete it.
        latch.trigger();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_created_before_trigger,
        )
        .await
        .expect("pre-registration trigger must be observed");

        latch.trigger();
        tokio::time::timeout(std::time::Duration::from_secs(1), latch.wait())
            .await
            .expect("future waits stay ready after repeated triggers");
    }

    #[test]
    fn register_writes_lifeline_and_reap_removes_live_app_record() {
        let dir = temp_dir("register");
        std::fs::create_dir_all(&dir).unwrap();

        register(&dir, std::process::id(), 999_999, "/tmp/ytm-ipc-test.sock");

        let path = registry_path(&dir);
        let text = safe_fs::read_to_string_no_symlink(&path).unwrap();
        let record: Lifeline = serde_json::from_str(&text).unwrap();
        assert_eq!(record.app_pid, std::process::id());
        assert_eq!(record.mpv_pid, 999_999);
        assert_eq!(record.mpv_socket, "/tmp/ytm-ipc-test.sock");
        assert!(record.written_at <= unix_now());

        reap_orphans(&dir);
        assert!(!path.exists(), "processed lifeline records are removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reap_orphans_discards_corrupt_and_stale_lifelines_without_killing() {
        let dir = temp_dir("bad-records");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);

        std::fs::write(&path, "{not json").unwrap();
        reap_orphans(&dir);
        assert!(!path.exists());

        let stale = Lifeline {
            app_pid: 999_991,
            mpv_pid: 999_992,
            mpv_socket: "/tmp/stale.sock".to_owned(),
            written_at: unix_now().saturating_sub(8 * 24 * 3600),
        };
        safe_fs::write_private_atomic_json(&path, &stale).unwrap();
        reap_orphans(&dir);
        assert!(
            !path.exists(),
            "stale lifeline is discarded before pid lookup"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn identity_matching_requires_socket_when_record_has_one() {
        let legacy = Lifeline {
            app_pid: 1,
            mpv_pid: 2,
            mpv_socket: String::new(),
            written_at: unix_now(),
        };
        assert!(mpv_identity_matches_args(&[], &legacy));

        let record = Lifeline {
            app_pid: 1,
            mpv_pid: 2,
            mpv_socket: "/tmp/ytm-ipc-abc.sock".to_owned(),
            written_at: unix_now(),
        };
        assert!(!mpv_identity_matches_args(&[], &record));
        assert!(!mpv_identity_matches_args(
            &["mpv".to_owned(), "--idle=yes".to_owned()],
            &record
        ));
        assert!(mpv_identity_matches_args(
            &[
                "mpv".to_owned(),
                "--input-ipc-server=/tmp/ytm-ipc-abc.sock".to_owned()
            ],
            &record
        ));
    }
}
