//! No-orphan process lifetime: mpv must die with the app, on every exit path.
//!
//! Every mpv is launched by [`super::guardian`]. The owner keeps a heartbeat plus a fixed-slot
//! emergency registry; the guardian owns the media process group, an inherited POSIX IPC lease,
//! and Linux parent-death protection or nested Windows Job Objects. Signals, panic, terminal-client
//! loss, owner/guardian death, and frozen-runtime timeouts all converge on that stable boundary.
//! An exact random command marker and kernel-pinned recovery handle form the next-start backstop.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

/// Every live media process, packed as `(guardian_pid << 32) | mpv_pid`.
///
/// The guardian is our direct child, so its pid cannot be reused before its `Child` is reaped. Every
/// out-of-band path targets only that stable guardian: Unix requests cooperative termination so the
/// guardian kills and reaps mpv itself, while Windows closes the last inner Job handle and
/// atomically kills mpv plus every descendant. The inherited POSIX IPC lease and Linux PDEATHSIG
/// separately cover direct guardian death. The recorded mpv pid is never signalled from this owner
/// registry, eliminating collateral kills after pid reuse. A fixed array keeps panic/signal
/// teardown allocation-free and supports concurrent audio/overlay instances.
const MEDIA_PID_SLOTS: usize = 16;
const SLOT_FREE: u8 = 0;
const SLOT_PUBLISHING: u8 = 1;
const SLOT_LIVE: u8 = 2;
const SLOT_OWNER_CLEANUP: u8 = 3;
const SLOT_EMERGENCY_KILL: u8 = 4;

/// The state word is the ownership handshake for the numeric guardian pid in `packed`.
///
/// Emergency teardown must claim `SLOT_LIVE -> SLOT_EMERGENCY_KILL` before loading or signalling
/// the pid. Normal owner teardown must claim `SLOT_LIVE -> SLOT_OWNER_CLEANUP` before it can reap
/// the direct guardian child. Therefore a guardian pid remains backed by the owner's live `Child`
/// handle for the entire raw-pid signal call, and can never be reused underneath that call.
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
        self.trigger_with_emergency(|| {});
    }

    /// Set the monotonic latch, run an emergency action, then wake async waiters.
    ///
    /// Terminal hard-failure paths use this ordering to close media admission and kill the global
    /// registry before a diagnostic mutex or a contended watch wake can delay teardown. Keeping the
    /// wake last also lets that path publish a best-effort exact diagnostic before the owner runs.
    pub(crate) fn trigger_with_emergency(&self, emergency: impl FnOnce()) {
        let first = !self.inner.triggered.swap(true, Ordering::AcqRel);
        emergency();
        if first {
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
#[cfg(unix)]
pub fn spawn_signal_handlers<F>(
    shutdown: ShutdownLatch,
    emit: F,
) -> std::io::Result<crate::util::background_task::BackgroundTask>
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
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
            tokio::select! {
                _ = hup.recv() => tracing::info!("received SIGHUP"),
                _ = term.recv() => tracing::info!("received SIGTERM"),
                _ = int.recv() => tracing::info!("received SIGINT"),
            }

            request_signal_shutdown(&shutdown, &emit);
        },
    ))
}

#[cfg(windows)]
pub fn spawn_signal_handlers<F>(
    shutdown: ShutdownLatch,
    emit: F,
) -> std::io::Result<crate::util::background_task::BackgroundTask>
where
    F: Fn(SignalEvent) + Send + Sync + 'static,
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
            tokio::select! {
                _ = ctrl_c.recv() => {}
                _ = ctrl_break.recv() => {}
                _ = ctrl_close.recv() => {}
                _ = ctrl_logoff.recv() => {}
                _ = ctrl_shutdown.recv() => {}
            }
            request_signal_shutdown(&shutdown, &emit);
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
    async fn shutdown_gate_permanently_rejects_a_late_guardian_registration() {
        let _pid_guard = lock_mpv_pid_for_test().await;
        kill_mpv_now();
        assert!(register_live_mpv(123_456, 123_456).is_err());
        reset_media_registry_for_test();
    }

    #[tokio::test]
    async fn blocked_guardian_slot_upgrades_to_the_actual_mpv_atomically() {
        let _pid_guard = lock_mpv_pid_for_test().await;
        let mut registration = register_live_mpv(123_456, 123_456).unwrap();
        registration.publish_mpv_pid(654_321).unwrap();
        assert_eq!(take_mpv_pid(), Some(654_321));
        drop(registration);
        reset_media_registry_for_test();
    }

    #[tokio::test]
    async fn owner_cannot_release_guardian_pid_during_emergency_signal_claim() {
        let _pid_guard = lock_mpv_pid_for_test().await;
        let registration = register_live_mpv(123_456, 654_321).unwrap();
        let entry = &MEDIA_PIDS[registration.slot];
        entry
            .state
            .compare_exchange(
                SLOT_LIVE,
                SLOT_EMERGENCY_KILL,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .expect("fixture claims emergency teardown");

        let (dropped_tx, dropped_rx) = std::sync::mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            drop(registration);
            let _ = dropped_tx.send(());
        });
        assert!(
            dropped_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "normal owner teardown must retain the Child/pid while emergency signalling owns it"
        );

        entry.packed.store(0, Ordering::SeqCst);
        entry.state.store(SLOT_FREE, Ordering::SeqCst);
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("owner teardown resumes after emergency signalling releases the pid");
        dropper.join().unwrap();
        reset_media_registry_for_test();
    }

    #[tokio::test]
    async fn stale_registration_drop_cannot_clear_a_reused_slot() {
        let _pid_guard = lock_mpv_pid_for_test().await;
        let mut stale = register_live_mpv(123_456, 654_321).unwrap();
        let slot = stale.slot;
        let entry = &MEDIA_PIDS[slot];
        entry
            .state
            .compare_exchange(
                SLOT_LIVE,
                SLOT_OWNER_CLEANUP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .expect("fixture claims normal owner teardown");
        // Model the exact tail of `terminate_claimed_slot` without signalling a made-up PID from
        // the test process: ownership must be revoked before SLOT_FREE becomes reusable.
        stale.active = false;
        entry.packed.store(0, Ordering::SeqCst);
        entry.state.store(SLOT_FREE, Ordering::SeqCst);

        let replacement = register_live_mpv(234_567, 765_432).unwrap();
        assert_eq!(replacement.slot, slot, "fixture must reuse the exact slot");
        let replacement_packed = replacement.packed;
        drop(stale);

        assert_eq!(entry.state.load(Ordering::SeqCst), SLOT_LIVE);
        assert_eq!(entry.packed.load(Ordering::SeqCst), replacement_packed);
        drop(replacement);
        reset_media_registry_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_kill_between_ready_and_mpv_publication_wins_fail_closed() {
        use std::process::Stdio;

        let _pid_guard = lock_mpv_pid_for_test().await;
        let mut command =
            crate::util::process::std_command("sleep", crate::util::process::ProcessProfile::Media);
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut guardian = command.spawn().expect("spawn blocked guardian fixture");
        let guardian_pid = guardian.id();
        let mut registration = register_live_mpv(guardian_pid, guardian_pid).unwrap();

        // Model the terminal watchdog firing after the real guardian emitted Ready but before the
        // owner published Ready's actual mpv pid into the temporary slot.
        kill_mpv_now();
        assert!(registration.publish_mpv_pid(654_321).is_err());
        guardian
            .wait()
            .expect("watchdog must terminate guardian fixture");

        drop(registration);
        reset_media_registry_for_test();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn out_of_band_shutdown_never_signals_the_recorded_actual_pid() {
        use std::process::Stdio;

        let _pid_guard = lock_mpv_pid_for_test().await;
        let spawn_fixture = || {
            let mut command = crate::util::process::std_command(
                "sleep",
                crate::util::process::ProcessProfile::Media,
            );
            command
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command.spawn().expect("spawn media registry fixture")
        };
        let mut unrelated_actual_pid = spawn_fixture();
        let mut guardian = spawn_fixture();
        let registration = register_live_mpv(unrelated_actual_pid.id(), guardian.id()).unwrap();

        kill_mpv_now();
        guardian
            .wait()
            .expect("out-of-band shutdown stops the guardian fixture");
        assert!(
            matches!(unrelated_actual_pid.try_wait(), Ok(None)),
            "owner registry must never signal the potentially reused actual pid"
        );

        let _ = unrelated_actual_pid.kill();
        let _ = unrelated_actual_pid.wait();
        drop(registration);
        reset_media_registry_for_test();
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
    fn v2_lifeline_retains_a_live_exact_app_identity() {
        let dir = temp_dir("register");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);
        let marker = "00112233445566778899aabbccddeeff";
        append_lifeline(
            path.clone(),
            Lifeline {
                app_pid: std::process::id(),
                app_started_at: current_process_started_at().unwrap(),
                mpv_pid: 999_999,
                mpv_socket: String::new(),
                identity_marker: marker.to_owned(),
                written_at: unix_now(),
            },
        )
        .unwrap();

        let text = safe_fs::read_to_string_no_symlink(&path).unwrap();
        let registry: LifelineRegistry = serde_json::from_str(&text).unwrap();
        assert_eq!(registry.version, 2);
        assert_eq!(registry.records.len(), 1);
        let record = &registry.records[0];
        assert_eq!(record.app_pid, std::process::id());
        assert_eq!(record.mpv_pid, 999_999);
        assert_eq!(record.identity_marker, marker);
        assert!(record.written_at <= unix_now());

        reap_orphans(&dir);
        assert!(path.exists(), "a live exact app identity is retained");
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
            app_started_at: 0,
            mpv_pid: 999_992,
            mpv_socket: "/tmp/stale.sock".to_owned(),
            identity_marker: String::new(),
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

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_identityless_record_never_kills_a_live_process() {
        use std::process::Stdio;

        let dir = temp_dir("legacy-no-authority");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);
        let mut command =
            crate::util::process::std_command("sleep", crate::util::process::ProcessProfile::Media);
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut candidate = command.spawn().expect("spawn legacy candidate");
        safe_fs::write_private_atomic_json(
            &path,
            &Lifeline {
                app_pid: u32::MAX,
                app_started_at: 0,
                mpv_pid: candidate.id(),
                mpv_socket: "/tmp/predictable-legacy.sock".to_owned(),
                identity_marker: String::new(),
                written_at: unix_now(),
            },
        )
        .unwrap();

        reap_orphans(&dir);
        assert!(!path.exists(), "legacy record is migrated away");
        assert!(
            matches!(candidate.try_wait(), Ok(None)),
            "a markerless record must never authorize termination"
        );

        let _ = candidate.kill();
        let _ = candidate.wait();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_marker_reaper_pins_kills_and_retains_until_gone() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        use sysinfo::{
            Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind,
        };

        struct ExactMarkerFixture {
            candidate: std::process::Child,
            dir: std::path::PathBuf,
        }

        impl Drop for ExactMarkerFixture {
            fn drop(&mut self) {
                let _ = self.candidate.kill();
                let _ = self.candidate.wait();
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        let dir = temp_dir("exact-reaper");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);
        let marker = "0123456789abcdef0123456789abcdef";
        let marker_arg = format!("--script-opts-append=yututui-lifeline={marker}");

        // bash's exec -a gives a single-process fixture whose argv contains the exact marker. The
        // replacement shell stops itself with a builtin, avoiding a helper process and avoiding
        // coreutils implementations which dispatch on argv[0] and reject the marker as a name.
        let mut command =
            crate::util::process::std_command("bash", crate::util::process::ProcessProfile::Media);
        command
            .arg("-c")
            .arg("exec -a \"$0\" bash -c 'kill -STOP $$'")
            .arg(&marker_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let candidate = command.spawn().expect("spawn exact-marker candidate");
        let mut fixture = ExactMarkerFixture { candidate, dir };
        let record = Lifeline {
            app_pid: u32::MAX,
            app_started_at: 1,
            mpv_pid: fixture.candidate.id(),
            mpv_socket: String::new(),
            identity_marker: marker.to_owned(),
            written_at: unix_now(),
        };

        // Bash can expose the final marker before it reaches the stopping builtin. A recovery pass
        // in that window can conservatively retain a transiently unavailable argv, so wait until
        // procfs reports both the exact marker and the final stopped state.
        let identity_deadline = Instant::now() + Duration::from_secs(2);
        let pid = Pid::from_u32(record.mpv_pid);
        let mut system = System::new();
        let identity_ready = loop {
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing()
                    .with_cmd(UpdateKind::Always)
                    .without_tasks(),
            );
            if system.process(pid).is_some_and(|process| {
                matches!(
                    process.status(),
                    ProcessStatus::Stop | ProcessStatus::Tracing
                ) && exact_identity(process, &record) == ExactIdentity::Match
            }) {
                break true;
            }
            if Instant::now() >= identity_deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            identity_ready,
            "the exact-marker fixture must expose its final stopped argv before recovery"
        );

        append_lifeline(path.clone(), record).unwrap();

        reap_orphans(&fixture.dir);
        assert!(
            path.exists(),
            "a signalled process stays recorded until a later observation proves it gone"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let exited = loop {
            match fixture.candidate.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => break false,
            }
        };
        assert!(exited, "the pinned exact-marker target must be terminated");

        reap_orphans(&fixture.dir);
        assert!(
            !path.exists(),
            "the next recovery pass removes a record after the target is gone"
        );
    }

    #[test]
    fn only_a_full_random_marker_grants_exact_identity() {
        let legacy = Lifeline {
            app_pid: 1,
            app_started_at: 0,
            mpv_pid: 2,
            mpv_socket: String::new(),
            identity_marker: String::new(),
            written_at: unix_now(),
        };
        assert!(!mpv_identity_matches_args(&[], &legacy));

        let record = Lifeline {
            app_pid: 1,
            app_started_at: 0,
            mpv_pid: 2,
            mpv_socket: "/tmp/ytm-ipc-abc.sock".to_owned(),
            identity_marker: String::new(),
            written_at: unix_now(),
        };
        assert!(!mpv_identity_matches_args(&[], &record));
        assert!(!mpv_identity_matches_args(
            &["mpv".to_owned(), "--idle=yes".to_owned()],
            &record
        ));
        assert!(!mpv_identity_matches_args(
            &[
                "mpv".to_owned(),
                "--input-ipc-server=/tmp/ytm-ipc-abc.sock".to_owned()
            ],
            &record
        ));

        let exact = Lifeline {
            app_pid: 1,
            app_started_at: 1,
            mpv_pid: 2,
            mpv_socket: String::new(),
            identity_marker: "00112233445566778899aabbccddeeff".to_owned(),
            written_at: 0,
        };
        assert!(mpv_identity_matches_args(
            &["--script-opts-append=yututui-lifeline=00112233445566778899aabbccddeeff".to_owned()],
            &exact,
        ));
        assert!(!mpv_identity_matches_args(
            &["--script-opts-append=yututui-lifeline=other".to_owned()],
            &exact,
        ));
        assert!(!mpv_identity_matches_args(
            &[
                "--script-opts-append=yututui-lifeline=00112233445566778899aabbccddeeff0"
                    .to_owned()
            ],
            &exact,
        ));
        assert!(!valid_identity_marker("00112233445566778899aabbccddeefg"));
        assert!(!valid_identity_marker("00112233445566778899aabbccddee"));
    }

    #[test]
    fn v2_registry_keeps_multiple_media_records() {
        let dir = temp_dir("multi-record");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);
        let record = |pid, marker: &str| Lifeline {
            app_pid: 101,
            app_started_at: 102,
            mpv_pid: pid,
            mpv_socket: String::new(),
            identity_marker: marker.to_owned(),
            written_at: unix_now(),
        };
        append_lifeline(
            path.clone(),
            record(201, "00112233445566778899aabbccddeeff"),
        )
        .unwrap();
        append_lifeline(
            path.clone(),
            record(202, "ffeeddccbbaa99887766554433221100"),
        )
        .unwrap();

        let records = read_lifeline_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.mpv_pid == 201));
        assert!(records.iter().any(|record| record.mpv_pid == 202));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exact_record_drop_preserves_other_guardian_and_legacy_migrates() {
        let dir = temp_dir("drop-one");
        std::fs::create_dir_all(&dir).unwrap();
        let path = registry_path(&dir);
        let legacy = Lifeline {
            app_pid: 10,
            app_started_at: 0,
            mpv_pid: 20,
            mpv_socket: "/tmp/legacy.sock".to_owned(),
            identity_marker: String::new(),
            written_at: unix_now(),
        };
        safe_fs::write_private_atomic_json(&path, &legacy).unwrap();

        let exact = |pid, marker: &str| Lifeline {
            app_pid: 30,
            app_started_at: 40,
            mpv_pid: pid,
            mpv_socket: String::new(),
            identity_marker: marker.to_owned(),
            written_at: unix_now(),
        };
        let marker_a = "00112233445566778899aabbccddeeff";
        let marker_b = "ffeeddccbbaa99887766554433221100";
        append_lifeline(path.clone(), exact(50, marker_a)).unwrap();
        append_lifeline(path.clone(), exact(51, marker_b)).unwrap();
        assert_eq!(read_lifeline_records(&path).unwrap().len(), 3);

        drop(DiskLifelineRegistration {
            path: path.clone(),
            app_pid: 30,
            mpv_pid: 50,
            identity_marker: marker_a.to_owned(),
        });
        let records = read_lifeline_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.mpv_pid == 20));
        assert!(records.iter().any(|record| record.mpv_pid == 51));

        drop(DiskLifelineRegistration {
            path: path.clone(),
            app_pid: 30,
            mpv_pid: 51,
            identity_marker: marker_b.to_owned(),
        });
        let records = read_lifeline_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].mpv_pid, 20);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lifeline_append_reports_durable_write_setup_failure() {
        let dir = temp_dir("write-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let non_directory = dir.join("not-a-directory");
        std::fs::write(&non_directory, b"block child creation").unwrap();
        let record = Lifeline {
            app_pid: 1,
            app_started_at: 2,
            mpv_pid: 3,
            mpv_socket: String::new(),
            identity_marker: "00112233445566778899aabbccddeeff".to_owned(),
            written_at: unix_now(),
        };
        assert!(append_lifeline(non_directory.join("registry.json"), record).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
