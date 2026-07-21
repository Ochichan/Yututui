//! No-orphan process lifetime: mpv must die with the app, on every exit path.
//!
//! Every mpv is launched by [`super::guardian`]. A heartbeat, fixed-slot emergency registry,
//! process-group/Job ownership, inherited POSIX lease, and parent-death protection ensure signals,
//! panics, terminal loss, owner death, and runtime freezes converge on one stable boundary.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

/// Every live media process, packed as `(guardian_pid << 32) | mpv_pid`.
///
/// Out-of-band paths target only the direct-child guardian, whose pid cannot be reused before reap.
/// Unix asks it to kill/reap mpv; Windows closes the inner Job handle to kill all descendants. The
/// fixed array keeps panic/signal teardown allocation-free for concurrent audio/overlay instances.
const MEDIA_PID_SLOTS: usize = 16;
const SLOT_FREE: u8 = 0;
const SLOT_PUBLISHING: u8 = 1;
const SLOT_LIVE: u8 = 2;
const SLOT_OWNER_CLEANUP: u8 = 3;
const SLOT_EMERGENCY_KILL: u8 = 4;

/// The state word hands off the guardian pid between normal and emergency teardown. Either side
/// must claim its transition before using it, keeping the direct-child handle live through signal.
struct MediaPidSlot {
    state: AtomicU8,
    packed: AtomicU64,
}

impl MediaPidSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(SLOT_FREE),
            packed: AtomicU64::new(0),
        }
    }
}

static MEDIA_PIDS: [MediaPidSlot; MEDIA_PID_SLOTS] =
    [const { MediaPidSlot::new() }; MEDIA_PID_SLOTS];
/// Monotonic fail-closed gate: once shutdown starts, no racing recovery path may publish a new
/// guardian after the kill-all scan has passed its slot.
static MEDIA_SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn pack_media_pid(mpv_pid: u32, termination_pid: u32) -> Option<u64> {
    if mpv_pid == 0 || termination_pid == 0 {
        return None;
    }
    Some((u64::from(termination_pid) << 32) | u64::from(mpv_pid))
}

fn unpack_mpv_pid(packed: u64) -> u32 {
    packed as u32
}

fn unpack_termination_pid(packed: u64) -> u32 {
    (packed >> 32) as u32
}

/// RAII ownership of one fixed-capacity media registry slot.
pub(crate) struct MediaPidRegistration {
    slot: usize,
    packed: u64,
    /// False once ownership has been transferred to emergency teardown or this registration has
    /// permanently freed its slot. A stale value may outlive that handoff while another media
    /// owner reuses the fixed slot, so `Drop` must never identify ownership by slot index alone.
    active: bool,
}

impl Drop for MediaPidRegistration {
    fn drop(&mut self) {
        if self.active {
            release_media_slot_for_owner(self.slot);
        }
    }
}

impl MediaPidRegistration {
    fn terminate_claimed_slot(&mut self, entry: &MediaPidSlot, packed: u64) {
        // Invalidate the old registration before publishing SLOT_FREE. Another media owner may
        // reuse this slot immediately after the store in `kill_owner_claimed_slot`; the eventual
        // Drop of this value must already be inert by then.
        self.active = false;
        kill_owner_claimed_slot(entry, packed);
    }

    /// Atomically replace a blocked guardian's temporary low word with the actual mpv pid.
    ///
    /// The guardian pid in the high word never changes. If global shutdown wins before, during,
    /// or immediately after this CAS, one side removes the slot and synchronously terminates the
    /// tree; callers must treat the error as a failed spawn and revoke their guardian lease.
    pub(crate) fn publish_mpv_pid(&mut self, mpv_pid: u32) -> std::io::Result<()> {
        let guardian_pid = unpack_termination_pid(self.packed);
        let replacement = pack_media_pid(mpv_pid, guardian_pid)
            .ok_or_else(|| std::io::Error::other("invalid zero media pid"))?;
        let entry = &MEDIA_PIDS[self.slot];
        if entry
            .state
            .compare_exchange(
                SLOT_LIVE,
                SLOT_OWNER_CLEANUP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            wait_for_emergency_kill(entry);
            self.active = false;
            return Err(std::io::Error::other(
                "media shutdown revoked guardian before mpv publication",
            ));
        }
        entry.packed.store(replacement, Ordering::SeqCst);
        self.packed = replacement;

        if MEDIA_SHUTDOWN.load(Ordering::SeqCst) {
            self.terminate_claimed_slot(entry, replacement);
            return Err(std::io::Error::other(
                "media shutdown raced mpv pid publication",
            ));
        }
        entry.state.store(SLOT_LIVE, Ordering::SeqCst);
        // If the shutdown scan passed while this slot was privately updating, take responsibility
        // for the process here. Otherwise the scan will claim the now-live slot itself.
        if MEDIA_SHUTDOWN.load(Ordering::SeqCst) {
            // Emergency teardown may publish SLOT_FREE before returning, so make this stale owner
            // inert first. If the emergency scan already won, it owns the same cleanup.
            self.active = false;
            claim_and_kill_media_slot(entry);
            return Err(std::io::Error::other(
                "media shutdown raced mpv pid publication",
            ));
        }
        Ok(())
    }

    /// Remove and synchronously terminate exactly this registered media tree.
    pub(crate) fn terminate_now(mut self) {
        let entry = &MEDIA_PIDS[self.slot];
        match entry.state.compare_exchange(
            SLOT_LIVE,
            SLOT_OWNER_CLEANUP,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => self.terminate_claimed_slot(entry, self.packed),
            Err(SLOT_EMERGENCY_KILL) => {
                wait_for_emergency_kill(entry);
                self.active = false;
            }
            Err(_) => self.active = false,
        }
    }
}

fn wait_for_emergency_kill(entry: &MediaPidSlot) {
    while entry.state.load(Ordering::SeqCst) == SLOT_EMERGENCY_KILL {
        std::hint::spin_loop();
        std::thread::yield_now();
    }
}

fn release_media_slot_for_owner(slot: usize) {
    let entry = &MEDIA_PIDS[slot];
    loop {
        match entry.state.compare_exchange(
            SLOT_LIVE,
            SLOT_OWNER_CLEANUP,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {
                entry.packed.store(0, Ordering::SeqCst);
                entry.state.store(SLOT_FREE, Ordering::SeqCst);
                return;
            }
            Err(SLOT_EMERGENCY_KILL) => wait_for_emergency_kill(entry),
            Err(SLOT_FREE) | Err(SLOT_OWNER_CLEANUP) => return,
            Err(SLOT_PUBLISHING) => {
                // A registration cannot be dropped by another thread while it is being published.
                // Treat this defensively like a short ownership handoff rather than releasing data
                // which a publisher still owns.
                std::hint::spin_loop();
                std::thread::yield_now();
            }
            Err(_) => return,
        }
    }
}

fn kill_owner_claimed_slot(entry: &MediaPidSlot, packed: u64) {
    kill_registered_media(unpack_mpv_pid(packed), unpack_termination_pid(packed));
    entry.packed.store(0, Ordering::SeqCst);
    entry.state.store(SLOT_FREE, Ordering::SeqCst);
}

fn claim_and_kill_media_slot(entry: &MediaPidSlot) -> bool {
    if entry
        .state
        .compare_exchange(
            SLOT_LIVE,
            SLOT_EMERGENCY_KILL,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return false;
    }
    let packed = entry.packed.load(Ordering::SeqCst);
    if packed != 0 {
        kill_registered_media(unpack_mpv_pid(packed), unpack_termination_pid(packed));
    }
    entry.packed.store(0, Ordering::SeqCst);
    entry.state.store(SLOT_FREE, Ordering::SeqCst);
    true
}

/// Register an actual mpv pid and the platform-specific process which synchronously owns its
/// whole tree. Playback fails closed if the bounded registry is unexpectedly exhausted.
pub(crate) fn register_live_mpv(
    mpv_pid: u32,
    termination_pid: u32,
) -> std::io::Result<MediaPidRegistration> {
    if MEDIA_SHUTDOWN.load(Ordering::SeqCst) {
        return Err(std::io::Error::other(
            "media shutdown is already in progress",
        ));
    }
    let packed = pack_media_pid(mpv_pid, termination_pid)
        .ok_or_else(|| std::io::Error::other("invalid zero media pid"))?;
    for (slot, entry) in MEDIA_PIDS.iter().enumerate() {
        if entry
            .state
            .compare_exchange(
                SLOT_FREE,
                SLOT_PUBLISHING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            entry.packed.store(packed, Ordering::SeqCst);
            entry.state.store(SLOT_LIVE, Ordering::SeqCst);
            // Pair with kill_mpv_now's store-before-scan. If shutdown raced the CAS, revoke this
            // exact process ourselves; either side wins, but the process can never survive.
            if MEDIA_SHUTDOWN.load(Ordering::SeqCst) {
                claim_and_kill_media_slot(entry);
                return Err(std::io::Error::other(
                    "media shutdown raced guardian startup",
                ));
            }
            return Ok(MediaPidRegistration {
                slot,
                packed,
                active: true,
            });
        }
    }
    Err(std::io::Error::other("live media pid registry is full"))
}

pub(crate) fn ensure_media_start_allowed() -> std::io::Result<()> {
    if MEDIA_SHUTDOWN.load(Ordering::SeqCst) {
        Err(std::io::Error::other(
            "media shutdown is already in progress",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
static MPV_PID_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn lock_mpv_pid_for_test() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = MPV_PID_TEST_LOCK.lock().await;
    reset_media_registry_for_test();
    guard
}

#[cfg(test)]
fn reset_media_registry_for_test() {
    for entry in &MEDIA_PIDS {
        entry.packed.store(0, Ordering::SeqCst);
        entry.state.store(SLOT_FREE, Ordering::SeqCst);
    }
    MEDIA_SHUTDOWN.store(false, Ordering::SeqCst);
}

/// Monotonic shutdown signal independent of bounded owner queues.
#[derive(Clone)]
pub struct ShutdownLatch {
    inner: Arc<ShutdownLatchInner>,
}

struct ShutdownLatchInner {
    state: AtomicU8,
    signal_won: AtomicBool,
    changed: tokio::sync::watch::Sender<bool>,
}

const SHUTDOWN_OPEN: u8 = 0;
const SHUTDOWN_CLAIMED: u8 = 1;
const SHUTDOWN_TRIGGERED: u8 = 2;

impl ShutdownLatch {
    pub fn new() -> Self {
        let (changed, _rx) = tokio::sync::watch::channel(false);
        Self {
            inner: Arc::new(ShutdownLatchInner {
                state: AtomicU8::new(SHUTDOWN_OPEN),
                signal_won: AtomicBool::new(false),
                changed,
            }),
        }
    }

    /// Set the latch once and wake every current or future waiter.
    pub fn trigger(&self) {
        self.trigger_with_emergency(|| {});
    }

    /// Set the latch, run an emergency action, then wake async waiters.
    pub(crate) fn trigger_with_emergency(&self, emergency: impl FnOnce()) {
        let _ = self.try_trigger_with_before_publish(|| {}, emergency);
    }

    /// Claim, publish the cause, expose shutdown, run emergency work, then wake async waiters.
    /// A loser cannot publish a competing cause or repeat the winner's emergency action.
    pub(crate) fn try_trigger_with_before_publish(
        &self,
        before_publish: impl FnOnce(),
        emergency: impl FnOnce(),
    ) -> bool {
        let claimed = self
            .inner
            .state
            .compare_exchange(
                SHUTDOWN_OPEN,
                SHUTDOWN_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if !claimed {
            return false;
        }
        before_publish();
        self.inner
            .state
            .store(SHUTDOWN_TRIGGERED, Ordering::Release);
        emergency();
        self.inner.changed.send_replace(true);
        true
    }

    pub fn is_triggered(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == SHUTDOWN_TRIGGERED
    }

    /// Whether an OS termination signal won the one-shot shutdown arbitration.
    pub fn was_triggered_by_signal(&self) -> bool {
        self.inner.signal_won.load(Ordering::Acquire)
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
            // Sender closure is impossible while this Arc is live; handle it defensively anyway.
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
    // Publish provenance and suppress recovery before killing mpv can enqueue TransportClosed.
    shutdown.try_trigger_with_before_publish(
        || shutdown.inner.signal_won.store(true, Ordering::Release),
        kill_mpv_now,
    );
    emit(SignalEvent::Quit);
}

/// Escalation state for the termination-signal task: the first signal starts the cooperative
/// shutdown, every later one demands a hard exit.
enum SignalPhase {
    Cooperative,
    Escalated,
}

/// Advance the two-phase signal lifecycle, shared by the Unix and Windows handlers and factored
/// out of the stream wiring so escalation ordering is unit-testable.
///
/// The first signal runs the cooperative path (latch + mpv kill + owner Quit event) and returns
/// `None`. That path depends on the owner loop actually observing the latch; if the owner is
/// wedged (deadlock, a blocked call), the process would otherwise ignore every later signal —
/// tokio keeps its low-level handlers installed, so once nothing consumes the streams the
/// signals are swallowed and only SIGKILL remains. A repeat signal therefore returns
/// `Some(exit_code)` and the caller must hard-exit.
fn advance_signal_phase<F>(
    phase: &mut SignalPhase,
    shutdown: &ShutdownLatch,
    emit: &F,
    name: &str,
    exit_code: i32,
) -> Option<i32>
where
    F: Fn(SignalEvent),
{
    match phase {
        SignalPhase::Cooperative => {
            tracing::info!("received {name}");
            request_signal_shutdown(shutdown, emit);
            *phase = SignalPhase::Escalated;
            None
        }
        SignalPhase::Escalated => {
            tracing::warn!(
                exit_code,
                "received {name} while cooperative shutdown was pending; forcing exit"
            );
            Some(exit_code)
        }
    }
}

/// Atomically take the recorded pid (leaving 0), so we kill it at most once.
#[cfg(test)]
fn take_mpv_pid() -> Option<u32> {
    for entry in &MEDIA_PIDS {
        if entry
            .state
            .compare_exchange(
                SLOT_LIVE,
                SLOT_OWNER_CLEANUP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            let packed = entry.packed.swap(0, Ordering::SeqCst);
            entry.state.store(SLOT_FREE, Ordering::SeqCst);
            if packed != 0 {
                return Some(unpack_mpv_pid(packed));
            }
        }
    }
    None
}

/// Kill the recorded mpv immediately, if any. Safe to call repeatedly.
pub fn kill_mpv_now() {
    MEDIA_SHUTDOWN.store(true, Ordering::SeqCst);
    for entry in &MEDIA_PIDS {
        claim_and_kill_media_slot(entry);
    }
}

#[cfg(unix)]
fn kill_registered_media(_mpv_pid: u32, guardian_pid: u32) {
    let Ok(guardian_pid) = libc::pid_t::try_from(guardian_pid) else {
        return;
    };
    // Never kill the guardian or signal the separately recorded mpv pid. The guardian is mpv's
    // only reliable reaper in a minimal container, so owner emergencies send its async-signal-safe
    // SIGTERM handler a cooperative request and leave it alive to terminate the media group and
    // synchronously wait. The guardian pid remains stable behind the owner's live Child handle for
    // this raw-pid signal call.
    crate::util::process::request_process_termination(guardian_pid);
}

#[cfg(windows)]
fn kill_registered_media(_mpv_pid: u32, guardian_pid: u32) {
    // The registered target is the guardian. Terminating it closes the last guardian-owned inner
    // Job handle, so Windows kills mpv and all helpers without allocating in the console handler.
    crate::util::process::terminate_process_id(guardian_pid);
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
///
/// `hard_exit` is the last-resort escape for a wedged owner loop: it runs only when a second
/// termination signal arrives while the cooperative shutdown is still pending, receives the
/// shell-convention exit code (`128 + signum` on Unix), and must not return (terminate the
/// process after any best-effort cleanup such as terminal restore).
#[cfg(unix)]
pub fn spawn_signal_handlers<F, H>(
    shutdown: ShutdownLatch,
    emit: F,
    hard_exit: H,
) -> std::io::Result<crate::util::background_task::BackgroundTask>
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
    H: FnOnce(i32) + Send + 'static,
{
    use tokio::signal::unix::{SignalKind, signal};

    // Construct streams now, before the caller can spawn mpv. Registration failure is a startup
    // error instead of an async task that quietly returns after playback has become unprotected.
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;

    Ok(crate::util::background_task::BackgroundTask::spawn(
        "termination signal handlers",
        async move {
            let mut phase = SignalPhase::Cooperative;
            let mut hard_exit = Some(hard_exit);
            loop {
                let (name, code) = tokio::select! {
                    _ = hup.recv() => ("SIGHUP", 129),
                    _ = term.recv() => ("SIGTERM", 143),
                    _ = int.recv() => ("SIGINT", 130),
                };
                if let Some(code) = advance_signal_phase(&mut phase, &shutdown, &emit, name, code)
                    && let Some(exit) = hard_exit.take()
                {
                    exit(code);
                    return;
                }
            }
        },
    ))
}

#[cfg(windows)]
pub fn spawn_signal_handlers<F, H>(
    shutdown: ShutdownLatch,
    emit: F,
    hard_exit: H,
) -> std::io::Result<crate::util::background_task::BackgroundTask>
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
    H: FnOnce(i32) + Send + 'static,
{
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};

    // Construct every stream synchronously, matching Unix's fail-closed startup contract. The
    // console callback remains the allocation-free emergency path; these streams coordinate the
    // ordinary owner shutdown latch and cleanup.
    let mut ctrl_c = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;
    let mut ctrl_close = ctrl_close()?;
    let mut ctrl_logoff = ctrl_logoff()?;
    let mut ctrl_shutdown = ctrl_shutdown()?;
    // Tokio installs one shared console handler while constructing the streams above. Windows
    // invokes handlers newest-first, so register the synchronous media kill only afterwards; it
    // must run before Tokio's close/logoff/shutdown handler parks its callback thread.
    install_console_ctrl_handler()?;
    Ok(crate::util::background_task::BackgroundTask::spawn(
        "termination signal handlers",
        async move {
            let mut phase = SignalPhase::Cooperative;
            let mut hard_exit = Some(hard_exit);
            loop {
                // Windows has no `128 + signum` convention; report every forced exit as 130
                // (the interrupt code shells already associate with a cancelled console app).
                let name = tokio::select! {
                    _ = ctrl_c.recv() => "CTRL_C",
                    _ = ctrl_break.recv() => "CTRL_BREAK",
                    _ = ctrl_close.recv() => "CTRL_CLOSE",
                    _ = ctrl_logoff.recv() => "CTRL_LOGOFF",
                    _ = ctrl_shutdown.recv() => "CTRL_SHUTDOWN",
                };
                if let Some(code) = advance_signal_phase(&mut phase, &shutdown, &emit, name, 130)
                    && let Some(exit) = hard_exit.take()
                {
                    exit(code);
                    return;
                }
            }
        },
    ))
}

/// Windows: register the newest console control handler so it kills media before Tokio's handler.
/// [`crate::util::process_guard::ChildTreeGuard`] already owns the kill-on-close Job Object; this
/// makes teardown prompt while the OS allows cleanup.
#[cfg(windows)]
fn install_console_ctrl_handler() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    // `windows-sys` models Win32 `BOOL` as a plain `i32` (1 = TRUE, 0 = FALSE).
    /// # Safety
    /// Called only by the Windows console subsystem with the documented control-event
    /// code; the handler does not dereference raw pointers or retain OS-owned state.
    // SAFETY: registered only through SetConsoleCtrlHandler with the required system
    // ABI; the body performs only atomic pid access and a best-effort kill.
    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        kill_mpv_now();
        // Continue to Tokio's already-registered handler. It publishes the async wake and, for
        // close/logoff/shutdown, keeps the OS callback alive while the owner performs cleanup.
        0
    }

    // SAFETY: `handler` has the required system ABI and static lifetime. Registration happens
    // after Tokio's handler so this callback is guaranteed to run first.
    unsafe {
        if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// On-disk record tying an mpv pid to the app instance that spawned it.
#[derive(Clone, Serialize, Deserialize)]
struct Lifeline {
    app_pid: u32,
    /// Process start time (sysinfo's platform-normalized seconds), preventing a reused app pid
    /// from suppressing recovery of an exact-marker v2 record.
    #[serde(default)]
    app_started_at: u64,
    mpv_pid: u32,
    /// Legacy IPC socket/pipe identity. Kept only so old records remain decodable; it never grants
    /// termination authority because a predictable path is not proof against PID reuse.
    #[serde(default)]
    mpv_socket: String,
    /// Guardian-generated identity present in every audio/overlay command line, including an
    /// overlay without a JSON IPC server. This is the v2 PID-reuse proof.
    #[serde(default)]
    identity_marker: String,
    /// Unix seconds when the record was written (i.e. at mpv spawn). This is retained only for
    /// decoding old files; exact-marker records do not rely on age for process identity.
    #[serde(default)]
    written_at: u64,
}

#[derive(Serialize, Deserialize)]
struct LifelineRegistry {
    version: u8,
    records: Vec<Lifeline>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LifelineFile {
    Registry(LifelineRegistry),
    Legacy(Lifeline),
}

static LIFELINE_FILE_LOCK: Mutex<()> = Mutex::new(());

/// Removes one normal-exit record without disturbing concurrently owned audio/overlay entries.
pub(crate) struct DiskLifelineRegistration {
    path: std::path::PathBuf,
    app_pid: u32,
    mpv_pid: u32,
    identity_marker: String,
}

impl Drop for DiskLifelineRegistration {
    fn drop(&mut self) {
        let _guard = LIFELINE_FILE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(_cross_process) = acquire_lifeline_file_lock(&self.path) else {
            // Retaining a stale exact-marker record is safer than overwriting another process's
            // update. The next writer/startup can remove it after observing no matching mpv.
            return;
        };
        let Some(mut records) = read_lifeline_records(&self.path) else {
            return;
        };
        records.retain(|record| {
            record.app_pid != self.app_pid
                || record.mpv_pid != self.mpv_pid
                || record.identity_marker != self.identity_marker
        });
        let _ = write_lifeline_records(&self.path, &records);
    }
}

fn registry_path(dir: &Path) -> std::path::PathBuf {
    dir.join("mpv-lifeline.json")
}

fn acquire_lifeline_file_lock(path: &Path) -> std::io::Result<safe_fs::AdvisoryFileLock> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("lifeline registry has no parent directory"))?;
    let lock_path = parent.join("mpv-lifeline.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match safe_fs::try_lock_private_file(&lock_path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "mpv lifeline registry remained locked",
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn current_process_started_at() -> std::io::Result<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    match system.process(pid).map(sysinfo::Process::start_time) {
        Some(started_at) if started_at != 0 => Ok(started_at),
        _ => Err(std::io::Error::other(
            "current process start identity is unavailable",
        )),
    }
}

/// Register a guardian-owned mpv only when this process holds the persistence writer lease.
/// Read-only `--new-instance` processes retain the in-memory/OS guarantees without mutating the
/// shared data root.
pub(crate) fn register_guarded_lifeline(
    mpv_pid: u32,
    identity_marker: &str,
) -> std::io::Result<Option<DiskLifelineRegistration>> {
    if mpv_pid == 0 || !valid_identity_marker(identity_marker) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "guarded mpv lifeline requires a nonzero pid and 128-bit hex marker",
        ));
    }
    if crate::persist::persistence_access().is_read_only() {
        return Ok(None);
    }
    let data_dir = crate::paths::data_dir()
        .ok_or_else(|| std::io::Error::other("mpv lifeline data directory is unavailable"))?;
    let path = registry_path(&data_dir);
    let app_pid = std::process::id();
    let app_started_at = current_process_started_at()?;
    append_lifeline(
        path.clone(),
        Lifeline {
            app_pid,
            app_started_at,
            mpv_pid,
            mpv_socket: String::new(),
            identity_marker: identity_marker.to_owned(),
            written_at: unix_now(),
        },
    )?;
    Ok(Some(DiskLifelineRegistration {
        path,
        app_pid,
        mpv_pid,
        identity_marker: identity_marker.to_owned(),
    }))
}

fn append_lifeline(path: std::path::PathBuf, record: Lifeline) -> std::io::Result<()> {
    let _guard = LIFELINE_FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _cross_process = acquire_lifeline_file_lock(&path)?;
    let mut records = read_lifeline_records(&path).unwrap_or_default();
    records.retain(|existing| {
        existing.app_pid != record.app_pid
            || existing.mpv_pid != record.mpv_pid
            || (!record.identity_marker.is_empty()
                && existing.identity_marker != record.identity_marker)
    });
    records.push(record);
    write_lifeline_records(&path, &records)
}

fn read_lifeline_records(path: &Path) -> Option<Vec<Lifeline>> {
    let data = safe_fs::read_to_string_no_symlink(path).ok()?;
    match serde_json::from_str::<LifelineFile>(&data).ok()? {
        LifelineFile::Registry(registry) if registry.version == 2 => Some(registry.records),
        LifelineFile::Registry(_) => None,
        LifelineFile::Legacy(record) => Some(vec![record]),
    }
}

fn write_lifeline_records(path: &Path, records: &[Lifeline]) -> std::io::Result<()> {
    if records.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let registry = LifelineRegistry {
        version: 2,
        records: records.to_vec(),
    };
    safe_fs::write_private_atomic_json(path, &registry)
}

/// Reap a previous instance's exact-marker mpv if that instance died without cleaning up.
///
/// Recovery never signals a numeric PID. Linux opens a pidfd and Windows opens a process handle
/// before refreshing argv, which binds validation and termination to one kernel process object.
/// Platforms without such a handle retain a matching record but do not risk collateral damage.
pub fn reap_orphans(dir: &Path) {
    let path = registry_path(dir);
    let _guard = LIFELINE_FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(_cross_process) = acquire_lifeline_file_lock(&path) else {
        return;
    };
    let Some(records) = read_lifeline_records(&path) else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let pids = records
        .iter()
        .map(|record| Pid::from_u32(record.app_pid))
        .collect::<Vec<_>>();
    sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);

    let mut retained = Vec::new();
    for record in records {
        // Old records had at most a socket/name check and no stable process handle. Decode them so
        // upgrades can clean the file, but never grant them authority to terminate anything.
        if !valid_identity_marker(&record.identity_marker)
            || record.app_started_at == 0
            || record.mpv_pid == 0
        {
            continue;
        }
        if sys
            .process(Pid::from_u32(record.app_pid))
            .is_some_and(|process| {
                record.app_started_at == 0 || process.start_time() == record.app_started_at
            })
        {
            retained.push(record);
            continue;
        }

        let stable = crate::util::process::open_stable_process(record.mpv_pid);
        if matches!(&stable, crate::util::process::StableProcessOpen::Gone) {
            continue;
        }

        // Refresh argv only after Linux/Windows pinned the target. If the PID had already been
        // reused, this snapshot belongs to the pinned replacement and cannot pass the random
        // marker check. Unsupported platforms use the same snapshot only to decide retention.
        let mpv_pid = Pid::from_u32(record.mpv_pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[mpv_pid]),
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .without_tasks(),
        );
        match sys
            .process(mpv_pid)
            .map(|process| exact_identity(process, &record))
        {
            None | Some(ExactIdentity::Unavailable) => {
                retained.push(record);
            }
            Some(ExactIdentity::Mismatch) => {}
            Some(ExactIdentity::Match) => match stable {
                crate::util::process::StableProcessOpen::Pinned(target) => {
                    match target.terminate_media() {
                        crate::util::process::StableProcessKill::Killed => {
                            tracing::warn!(
                                mpv_pid = record.mpv_pid,
                                "terminated exact orphaned mpv from a prior run"
                            );
                            // Termination is asynchronous on Windows and an uninterruptible Unix
                            // task can outlive SIGKILL. Keep the marker until a later run proves the
                            // pinned process is gone.
                            retained.push(record);
                        }
                        crate::util::process::StableProcessKill::Gone => {}
                        crate::util::process::StableProcessKill::Retry(error) => {
                            tracing::warn!(
                                %error,
                                mpv_pid = record.mpv_pid,
                                "exact orphaned mpv could not be terminated; retaining lifeline"
                            );
                            retained.push(record);
                        }
                    }
                }
                crate::util::process::StableProcessOpen::Unavailable(error) => {
                    tracing::warn!(
                        %error,
                        mpv_pid = record.mpv_pid,
                        "exact orphaned mpv cannot be signalled safely on this platform; retaining lifeline"
                    );
                    retained.push(record);
                }
                crate::util::process::StableProcessOpen::Gone => {}
            },
        }
    }
    let _ = write_lifeline_records(&path, &retained);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExactIdentity {
    Match,
    Mismatch,
    Unavailable,
}

fn valid_identity_marker(marker: &str) -> bool {
    marker.len() == 32 && marker.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Confirm that a pinned candidate carries the full random guardian marker in one argv element.
fn exact_identity(proc_: &sysinfo::Process, record: &Lifeline) -> ExactIdentity {
    let args: Vec<String> = proc_
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if args.is_empty() {
        ExactIdentity::Unavailable
    } else if mpv_identity_matches_args(&args, record) {
        ExactIdentity::Match
    } else {
        ExactIdentity::Mismatch
    }
}

fn mpv_identity_matches_args(args: &[String], record: &Lifeline) -> bool {
    if !valid_identity_marker(&record.identity_marker) {
        return false;
    }
    args.iter().any(|arg| {
        arg.strip_prefix("--script-opts-append=yututui-lifeline=")
            == Some(record.identity_marker.as_str())
    })
}

#[cfg(test)]
mod tests;
