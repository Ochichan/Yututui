//! Single-instance lock for the GUI *process itself* (docs/gui/03 §6).
//!
//! Entirely separate from the core's `InstanceFile`/primary socket — the GUI never binds the
//! primary socket and never writes an `InstanceFile`. Windows uses a named mutex; unix uses
//! `flock` on a lockfile in the runtime dir. A second launch signals the first (which shows +
//! focuses the main window) over a small per-user activate endpoint, then exits.

use std::collections::VecDeque;
use std::io;

#[cfg(unix)]
use std::path::PathBuf;

use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::util::runtime;

const ACTIVATE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const ACTIVATE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const ACTIVATE_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const ACTIVATE_FRAME_MAX_BYTES: usize = 32;
const ACTIVATE_ACK_MAX_BYTES: usize = 16;
const ACTIVATE_MAX_CONNECTIONS: usize = 16;
const PENDING_ACTIVATION_CAPACITY: usize = 32;

static ACTIVATION_LISTENER: std::sync::Mutex<Option<ActivationListener>> =
    std::sync::Mutex::new(None);

/// What a secondary desktop invocation wants the already-running process to do.
///
/// This stays deliberately independent of CLI parsing so startup entries, native launchers,
/// and future deep links can all share the same single-instance transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationIntent {
    EnsureTray,
    ShowMini,
    ShowMain,
}

impl ActivationIntent {
    fn wire(self) -> &'static str {
        match self {
            Self::EnsureTray => "ensure_tray",
            Self::ShowMini => "show_mini",
            Self::ShowMain => "show_main",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "ensure_tray" => Some(Self::EnsureTray),
            "show_mini" => Some(Self::ShowMini),
            "show_main" | "activate" => Some(Self::ShowMain),
            _ => None,
        }
    }
}

/// Bridges the short interval between binding the activation endpoint and creating the native
/// event-loop proxy.
///
/// The endpoint must be live as soon as the primary owns the process lock, but both tao backends
/// create their proxy only after startup repair. Requests accepted during that interval are
/// acknowledged only when they fit in this bounded FIFO. Installing the proxy drains the FIFO
/// while holding the same lock, so a later request cannot overtake an earlier queued request.
pub(crate) struct DeferredActivations<T> {
    state: std::sync::Arc<std::sync::Mutex<DeferredActivationState<T>>>,
}

struct DeferredActivationState<T> {
    target: Option<T>,
    pending: VecDeque<ActivationIntent>,
}

impl<T> Clone for DeferredActivations<T> {
    fn clone(&self) -> Self {
        Self {
            state: std::sync::Arc::clone(&self.state),
        }
    }
}

impl<T> DeferredActivations<T> {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(DeferredActivationState {
                target: None,
                pending: VecDeque::new(),
            })),
        }
    }

    /// Deliver immediately when the target exists, or enqueue before startup completes.
    /// Returning `false` makes the listener send a truthful `rejected` acknowledgement.
    pub(crate) fn deliver_or_defer<F>(&self, intent: ActivationIntent, deliver: F) -> bool
    where
        F: FnOnce(&T, ActivationIntent) -> bool,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(target) = state.target.as_ref() {
            return deliver(target, intent);
        }
        if state.pending.len() >= PENDING_ACTIVATION_CAPACITY {
            return false;
        }
        state.pending.push_back(intent);
        true
    }

    /// Install the live delivery target and drain every pre-loop intent in FIFO order.
    ///
    /// The return value is the number of queued intents that could not be posted because the
    /// target had already closed. At normal startup this is always zero.
    pub(crate) fn install<F>(&self, target: T, deliver: F) -> usize
    where
        F: Fn(&T, ActivationIntent) -> bool,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.target = Some(target);
        let mut rejected = 0;
        while let Some(intent) = state.pending.pop_front() {
            if !deliver(
                state
                    .target
                    .as_ref()
                    .expect("activation target was installed before draining"),
                intent,
            ) {
                rejected += 1;
            }
        }
        rejected
    }
}

/// The result of trying to become the single GUI instance.
pub enum Acquire {
    /// We are the first instance; hold this guard for the whole process lifetime.
    Primary(InstanceGuard),
    /// Another GUI instance already owns the lock.
    AlreadyRunning,
}

/// RAII guard that releases the lock on drop (unix: explicitly unlocks the flocked fd;
/// Windows: closes the mutex handle).
pub struct InstanceGuard {
    #[cfg(unix)]
    _lock: std::fs::File,
    #[cfg(windows)]
    _handle: WindowsMutex,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Stop accepting activations before releasing the process lock. Otherwise a new
        // primary can acquire the lock while the old endpoint still acknowledges requests.
        shutdown_activation_listener();
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;

            // Explicit unlock matters if this open-file description was duplicated or inherited.
            // SAFETY: `_lock` owns a valid fd throughout Drop; close remains the fallback.
            let _ = unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(windows)]
struct WindowsMutex(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for WindowsMutex {
    fn drop(&mut self) {
        // SAFETY: `WindowsMutex` owns this handle and drops it once when the guard
        // leaves scope; CloseHandle failure only means the handle was already invalid.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
#[cfg(windows)]
/// # Safety
/// The mutex handle is owned by the guard and is never concurrently accessed through
/// shared Rust references; moving the owner to another thread preserves ownership.
// SAFETY: `WindowsMutex` has unique ownership of the raw HANDLE and Drop closes it.
unsafe impl Send for WindowsMutex {}

#[cfg(unix)]
pub fn acquire() -> io::Result<Acquire> {
    use std::os::unix::io::AsRawFd;
    let path = lock_path()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // Advisory, non-blocking exclusive lock on the open file description.
    // SAFETY: `file.as_raw_fd()` is valid for the lifetime of `file`; flock reports
    // contention or OS errors via its return value.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Acquire::Primary(InstanceGuard { _lock: file }))
    } else {
        let err = io::Error::last_os_error();
        // flock sets EWOULDBLOCK when LOCK_NB would otherwise block (another holder).
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(Acquire::AlreadyRunning)
        } else {
            Err(err)
        }
    }
}

#[cfg(windows)]
pub fn acquire() -> io::Result<Acquire> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = mutex_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: the mutex name is NUL-terminated and lives for the call; null security
    // attributes request defaults and errors are checked via the returned handle.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `GetLastError` is read immediately after CreateMutexW on this thread,
    // as required for detecting an existing named mutex.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: this branch does not take ownership for the process; close the
        // newly opened mutex handle before reporting AlreadyRunning.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        Ok(Acquire::AlreadyRunning)
    } else {
        Ok(Acquire::Primary(InstanceGuard {
            _handle: WindowsMutex(handle),
        }))
    }
}

#[cfg(unix)]
fn lock_path() -> io::Result<PathBuf> {
    Ok(runtime::app_runtime_dir()?.join(format!(
        "yututui-desktop-{}.lock",
        runtime::filesystem_user_tag()
    )))
}

/// Per-user endpoint the second instance pings to activate the first.
fn activate_endpoint() -> io::Result<String> {
    let tag = runtime::filesystem_user_tag();
    #[cfg(windows)]
    {
        Ok(format!(
            r"\\.\pipe\yututui-desktop-activate-{tag}-session-{}",
            windows_session_id()
        ))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()?
            .join(format!("yututui-desktop-activate-{tag}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

#[cfg(windows)]
fn windows_session_id() -> u32 {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    // `ProcessIdToSessionId` is exported by kernel32 on every supported Windows version.
    // Declare the one stable ABI directly so session-scoping does not require changing the
    // dependency feature set (desktop hardening must not perturb Cargo.lock/dependencies).
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "ProcessIdToSessionId"]
        fn process_id_to_session_id(process_id: u32, session_id: *mut u32) -> i32;
    }

    let mut session = 0;
    // SAFETY: `session` is a valid out pointer for the duration of the call. Falling back to
    // zero still keeps the mutex and activation endpoint in the same namespace.
    if unsafe { process_id_to_session_id(GetCurrentProcessId(), &mut session) } == 0 {
        0
    } else {
        session
    }
}

#[cfg(windows)]
fn mutex_name() -> String {
    format!(
        r"Local\io.github.ochi.yututui.desktop.user.{}.session.{}",
        runtime::filesystem_user_tag(),
        windows_session_id()
    )
}

/// Result adapter for activation callbacks.
///
/// Existing callbacks returning `()` remain source-compatible and mean “accepted”. New callers
/// should return the event-loop proxy's `Result<(), _>` (or a `bool`) so a closed native loop is
/// reported to the secondary instead of receiving a false-positive acknowledgement.
pub trait IntoActivationDelivery {
    fn delivered(self) -> bool;
}

impl IntoActivationDelivery for () {
    fn delivered(self) -> bool {
        true
    }
}

impl IntoActivationDelivery for bool {
    fn delivered(self) -> bool {
        self
    }
}

impl<E> IntoActivationDelivery for Result<(), E> {
    fn delivered(self) -> bool {
        self.is_ok()
    }
}

/// Owned activation listener. Dropping it requests shutdown, waits for the listener thread up to
/// a fixed deadline, and joins it when it exits. The process-global compatibility entry point
/// stores one of these until the [`InstanceGuard`] is dropped.
pub struct ActivationListener {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<std::sync::mpsc::Receiver<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ActivationListener {
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let stopped = self
            .done
            .take()
            .is_none_or(|done| done.recv_timeout(ACTIVATE_SHUTDOWN_TIMEOUT).is_ok());
        if stopped {
            if let Some(thread) = self.thread.take()
                && thread.join().is_err()
            {
                tracing::warn!(target: "ytt_desktop", "activation listener thread panicked");
            }
        } else {
            tracing::warn!(target: "ytt_desktop", "activation listener did not stop before deadline");
            // Rust cannot safely force-stop an OS thread. Dropping the join handle detaches it;
            // the closed shutdown channel still prevents another accepted activation afterward.
            self.thread.take();
        }
    }
}

impl Drop for ActivationListener {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

async fn read_newline_frame<R>(reader: &mut R, max_bytes: usize) -> io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut frame = Vec::with_capacity(max_bytes.min(64));
    let mut chunk = [0_u8; 16];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "activation frame ended before newline",
            ));
        }
        if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            if frame.len() + newline > max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "activation frame is too large",
                ));
            }
            frame.extend_from_slice(&chunk[..newline]);
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return String::from_utf8(frame).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "activation frame is not UTF-8")
            });
        }
        if frame.len() + read > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "activation frame is too large",
            ));
        }
        frame.extend_from_slice(&chunk[..read]);
    }
}

async fn write_newline_frame<W>(writer: &mut W, value: &str, max_bytes: usize) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if value.len() > max_bytes || value.as_bytes().contains(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid activation frame",
        ));
    }
    writer.write_all(value.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn serve_activation_connection<S, F, R>(conn: &mut S, on_activate: &F) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: Fn(ActivationIntent) -> R,
    R: IntoActivationDelivery,
{
    let frame = match read_newline_frame(conn, ACTIVATE_FRAME_MAX_BYTES).await {
        Ok(frame) => frame,
        Err(error) if matches!(error.kind(), io::ErrorKind::InvalidData) => {
            return write_newline_frame(conn, "invalid", ACTIVATE_ACK_MAX_BYTES).await;
        }
        Err(error) => return Err(error),
    };
    let Some(intent) = ActivationIntent::from_wire(&frame) else {
        return write_newline_frame(conn, "invalid", ACTIVATE_ACK_MAX_BYTES).await;
    };
    let ack = if on_activate(intent).delivered() {
        "ok"
    } else {
        "rejected"
    };
    write_newline_frame(conn, ack, ACTIVATE_ACK_MAX_BYTES).await
}

fn spawn_activation_listener_at<F, R>(
    endpoint: String,
    on_activate: F,
) -> io::Result<ActivationListener>
where
    F: Fn(ActivationIntent) -> R + Send + Sync + 'static,
    R: IntoActivationDelivery,
{
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("yututray-activate".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(runtime) = runtime else {
                let _ = ready_tx.send(Err((
                    io::ErrorKind::Other,
                    "could not create activation runtime".to_string(),
                )));
                let _ = done_tx.send(());
                return;
            };
            #[cfg(unix)]
            let cleanup_endpoint = endpoint.clone();
            runtime.block_on(async move {
                // Clear a stale unix socket file (no-op/harmless on Windows pipes).
                #[cfg(unix)]
                let _ = std::fs::remove_file(&endpoint);
                let Ok(name) = endpoint.as_str().to_fs_name::<GenericFilePath>() else {
                    let _ = ready_tx.send(Err((
                        io::ErrorKind::InvalidInput,
                        "invalid activation endpoint".to_string(),
                    )));
                    return;
                };
                let listener = match ListenerOptions::new().name(name).create_tokio() {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_tx.send(Err((error.kind(), error.to_string())));
                        return;
                    }
                };
                // Do not return to native event-loop setup until the endpoint is accepting.
                let _ = ready_tx.send(Ok(()));
                let on_activate = std::sync::Arc::new(on_activate);
                let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(
                    ACTIVATE_MAX_CONNECTIONS,
                ));
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let accepted = tokio::select! {
                        _ = &mut shutdown_rx => {
                            connections.abort_all();
                            while connections.join_next().await.is_some() {}
                            break;
                        }
                        completed = connections.join_next(), if !connections.is_empty() => {
                            if let Some(Err(error)) = completed {
                                tracing::debug!(target: "ytt_desktop", %error, "activation connection task failed");
                            }
                            continue;
                        }
                        accepted = listener.accept() => accepted,
                    };
                    let mut conn = match accepted {
                        Ok(conn) => conn,
                        Err(error) => {
                            tracing::warn!(target: "ytt_desktop", %error, "activation listener accept failed");
                            break;
                        }
                    };
                    let Ok(permit) = std::sync::Arc::clone(&permits).try_acquire_owned() else {
                        tracing::warn!(target: "ytt_desktop", "activation connection limit reached");
                        continue;
                    };
                    let on_activate = std::sync::Arc::clone(&on_activate);
                    connections.spawn(async move {
                        let _permit = permit;
                        let served = tokio::time::timeout(
                            ACTIVATE_CONNECT_TIMEOUT,
                            serve_activation_connection(&mut conn, on_activate.as_ref()),
                        )
                        .await;
                        if let Ok(Err(error)) = served {
                            tracing::debug!(target: "ytt_desktop", %error, "activation request failed");
                        }
                    });
                }
            });
            #[cfg(unix)]
            let _ = std::fs::remove_file(cleanup_endpoint);
            let _ = done_tx.send(());
        })?;

    let mut handle = ActivationListener {
        shutdown: Some(shutdown_tx),
        done: Some(done_rx),
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(ACTIVATE_READY_TIMEOUT) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err((kind, message))) => {
            handle.stop_inner();
            Err(io::Error::new(kind, message))
        }
        Err(_) => {
            handle.stop_inner();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "activation listener did not become ready",
            ))
        }
    }
}

/// Spawn an owned listener for callers that want to control its lifetime directly.
pub fn spawn_activation_listener_handle<F, R>(on_activate: F) -> io::Result<ActivationListener>
where
    F: Fn(ActivationIntent) -> R + Send + Sync + 'static,
    R: IntoActivationDelivery,
{
    spawn_activation_listener_at(activate_endpoint()?, on_activate)
}

/// First instance: accept activate signals on a dedicated thread, calling `on_activate`
/// (which posts a show/focus request to the event loop) for each. The process-global owner keeps
/// the listener alive and the [`InstanceGuard`] shuts it down before releasing the lock.
pub fn spawn_activation_listener<F, R>(on_activate: F) -> io::Result<()>
where
    F: Fn(ActivationIntent) -> R + Send + Sync + 'static,
    R: IntoActivationDelivery,
{
    let mut slot = ACTIVATION_LISTENER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation listener is already running",
        ));
    }
    *slot = Some(spawn_activation_listener_handle(on_activate)?);
    Ok(())
}

/// Stop and join the process-global activation listener, if one is running.
pub fn shutdown_activation_listener() {
    let listener = ACTIVATION_LISTENER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(listener);
}

/// Compatibility wrapper for callers that have not adopted explicit activation intents yet.
pub fn spawn_activate_listener<F, R>(on_activate: F) -> io::Result<()>
where
    F: Fn() -> R + Send + Sync + 'static,
    R: IntoActivationDelivery,
{
    spawn_activation_listener(move |_| on_activate())
}

/// Second instance: deliver an intent and wait for the primary to acknowledge it.
pub fn signal_activation(intent: ActivationIntent) -> io::Result<()> {
    let endpoint = activate_endpoint()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let deadline = tokio::time::Instant::now() + ACTIVATE_CONNECT_TIMEOUT;
        let mut last_error = None;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(last_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "activation request timed out")
                }));
            }
            let name = endpoint
                .as_str()
                .to_fs_name::<GenericFilePath>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid endpoint"))?;
            match tokio::time::timeout_at(deadline, Stream::connect(name)).await {
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "activation connection timed out",
                    ));
                }
                Ok(Ok(mut conn)) => {
                    let exchange = async {
                        write_newline_frame(&mut conn, intent.wire(), ACTIVATE_FRAME_MAX_BYTES)
                            .await?;
                        read_newline_frame(&mut conn, ACTIVATE_ACK_MAX_BYTES).await
                    };
                    let ack =
                        tokio::time::timeout_at(deadline, exchange)
                            .await
                            .map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "activation acknowledgement timed out",
                                )
                            })??;
                    return match ack.as_str() {
                        "ok" => Ok(()),
                        "rejected" => Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "primary could not accept activation request",
                        )),
                        _ => Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "primary rejected activation request",
                        )),
                    };
                }
                Ok(Err(error)) => {
                    last_error = Some(error);
                }
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(50)).min(deadline),
            )
            .await;
        }
    })
}

/// Compatibility wrapper preserving the former “surface the main window” behavior.
pub fn signal_activate() {
    let _ = signal_activation(ActivationIntent::ShowMain);
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::io::AsyncWriteExt;

    use super::*;

    static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn activation_intents_round_trip_and_reject_unknown_values() {
        for intent in [
            ActivationIntent::EnsureTray,
            ActivationIntent::ShowMini,
            ActivationIntent::ShowMain,
        ] {
            assert_eq!(ActivationIntent::from_wire(intent.wire()), Some(intent));
        }
        assert_eq!(
            ActivationIntent::from_wire("activate"),
            Some(ActivationIntent::ShowMain)
        );
        assert_eq!(ActivationIntent::from_wire("surprise"), None);
    }

    #[test]
    fn deferred_activations_are_bounded_and_drained_in_fifo_order() {
        let activations = DeferredActivations::<Arc<Mutex<Vec<ActivationIntent>>>>::new();
        for index in 0..PENDING_ACTIVATION_CAPACITY {
            let intent = if index % 2 == 0 {
                ActivationIntent::ShowMini
            } else {
                ActivationIntent::ShowMain
            };
            assert!(activations.deliver_or_defer(intent, |_, _| false));
        }
        assert!(
            !activations.deliver_or_defer(ActivationIntent::EnsureTray, |_, _| true),
            "a full pre-loop FIFO must reject instead of growing without bound"
        );

        let seen = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(
            activations.install(Arc::clone(&seen), |target, intent| {
                target
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(intent);
                true
            }),
            0
        );
        assert!(
            activations.deliver_or_defer(ActivationIntent::EnsureTray, |target, intent| {
                target
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(intent);
                true
            })
        );

        let seen = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(seen.len(), PENDING_ACTIVATION_CAPACITY + 1);
        assert_eq!(seen[0], ActivationIntent::ShowMini);
        assert_eq!(seen[1], ActivationIntent::ShowMain);
        assert_eq!(
            seen[PENDING_ACTIVATION_CAPACITY],
            ActivationIntent::EnsureTray
        );
    }

    #[tokio::test]
    async fn newline_frame_reassembles_fragmented_reads() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let write = async move {
            writer.write_all(b"show_").await?;
            tokio::task::yield_now().await;
            writer.write_all(b"mi").await?;
            tokio::task::yield_now().await;
            writer.write_all(b"ni\n").await
        };
        let read = read_newline_frame(&mut reader, ACTIVATE_FRAME_MAX_BYTES);
        let (write_result, frame) = tokio::join!(write, read);
        write_result.unwrap();
        assert_eq!(frame.unwrap(), "show_mini");
    }

    #[tokio::test]
    async fn newline_frame_rejects_oversized_input_without_unbounded_buffering() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let write = async move {
            let oversized = [b'x'; ACTIVATE_FRAME_MAX_BYTES + 1];
            writer.write_all(&oversized).await?;
            writer.write_all(b"\n").await
        };
        let read = read_newline_frame(&mut reader, ACTIVATE_FRAME_MAX_BYTES);
        let (write_result, frame) = tokio::join!(write, read);
        write_result.unwrap();
        assert_eq!(frame.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn acknowledgement_requires_callback_delivery() {
        for (delivered, expected_ack) in [(true, "ok"), (false, "rejected")] {
            let (mut client, mut server) = tokio::io::duplex(64);
            let callback = |intent| {
                assert_eq!(intent, ActivationIntent::ShowMain);
                delivered.then_some(()).ok_or("event loop closed")
            };
            let serve = serve_activation_connection(&mut server, &callback);
            let exchange = async {
                write_newline_frame(
                    &mut client,
                    ActivationIntent::ShowMain.wire(),
                    ACTIVATE_FRAME_MAX_BYTES,
                )
                .await?;
                read_newline_frame(&mut client, ACTIVATE_ACK_MAX_BYTES).await
            };
            let (serve_result, ack) = tokio::join!(serve, exchange);
            serve_result.unwrap();
            assert_eq!(ack.unwrap(), expected_ack);
        }
    }

    #[tokio::test]
    async fn owned_listener_accepts_fragmented_intent_and_joins_on_stop() {
        let serial = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ytt-si-transport-{}-{serial}.sock",
            std::process::id()
        ));
        let endpoint = path.to_string_lossy().into_owned();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_callback = Arc::clone(&seen);
        let listener = spawn_activation_listener_at(endpoint.clone(), move |intent| {
            seen_in_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(intent);
            Ok::<(), ()>(())
        })
        .unwrap();

        let name = endpoint
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .expect("test endpoint is a valid local-socket path");
        let mut conn = Stream::connect(name).await.unwrap();
        conn.write_all(b"show_").await.unwrap();
        tokio::task::yield_now().await;
        conn.write_all(b"mini\n").await.unwrap();
        conn.flush().await.unwrap();
        assert_eq!(
            read_newline_frame(&mut conn, ACTIVATE_ACK_MAX_BYTES)
                .await
                .unwrap(),
            "ok"
        );

        listener.stop();
        assert_eq!(
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![ActivationIntent::ShowMini]
        );
        assert!(!path.exists(), "listener shutdown should remove its socket");
    }

    #[tokio::test]
    async fn slow_connection_does_not_block_a_valid_second_activation() {
        let serial = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ytt-si-concurrent-{}-{serial}.sock",
            std::process::id()
        ));
        let endpoint = path.to_string_lossy().into_owned();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_callback = Arc::clone(&seen);
        let listener = spawn_activation_listener_at(endpoint.clone(), move |intent| {
            seen_in_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(intent);
            true
        })
        .unwrap();

        let name = endpoint
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .expect("test endpoint is a valid local-socket path");
        let mut slow = Stream::connect(name.clone()).await.unwrap();
        slow.write_all(b"show_").await.unwrap();
        slow.flush().await.unwrap();
        // Give the listener enough time to enter the first connection's frame read. The old
        // serial listener then waited its full one-second timeout before accepting another peer.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut fast = Stream::connect(name).await.unwrap();
        let fast_exchange = async {
            write_newline_frame(
                &mut fast,
                ActivationIntent::ShowMain.wire(),
                ACTIVATE_FRAME_MAX_BYTES,
            )
            .await?;
            read_newline_frame(&mut fast, ACTIVATE_ACK_MAX_BYTES).await
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(750), fast_exchange)
                .await
                .expect("a slow peer must not head-of-line block a valid activation")
                .unwrap(),
            "ok"
        );

        drop(slow);
        listener.stop();
        assert_eq!(
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![ActivationIntent::ShowMain]
        );
        assert!(!path.exists(), "listener shutdown should remove its socket");
    }

    #[tokio::test]
    async fn ten_concurrent_secondaries_each_receive_one_acknowledgement() {
        let serial = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ytt-si-ten-secondaries-{}-{serial}.sock",
            std::process::id()
        ));
        let endpoint = path.to_string_lossy().into_owned();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_in_callback = Arc::clone(&seen);
        let listener = spawn_activation_listener_at(endpoint.clone(), move |intent| {
            assert_eq!(intent, ActivationIntent::ShowMini);
            seen_in_callback.fetch_add(1, Ordering::AcqRel);
            true
        })
        .unwrap();

        let mut secondaries = Vec::new();
        for _ in 0..10 {
            let endpoint = endpoint.clone();
            secondaries.push(tokio::spawn(async move {
                let name = endpoint
                    .as_str()
                    .to_fs_name::<GenericFilePath>()
                    .expect("test endpoint is a valid local-socket path");
                let mut conn = Stream::connect(name).await?;
                write_newline_frame(
                    &mut conn,
                    ActivationIntent::ShowMini.wire(),
                    ACTIVATE_FRAME_MAX_BYTES,
                )
                .await?;
                read_newline_frame(&mut conn, ACTIVATE_ACK_MAX_BYTES).await
            }));
        }
        for secondary in secondaries {
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(1), secondary)
                    .await
                    .expect("secondary acknowledgement timed out")
                    .expect("secondary task panicked")
                    .expect("activation exchange failed"),
                "ok"
            );
        }

        listener.stop();
        assert_eq!(seen.load(Ordering::Acquire), 10);
        assert!(!path.exists(), "listener shutdown should remove its socket");
    }

    #[test]
    fn exclusive_flock_blocks_a_second_holder() {
        let path = std::env::temp_dir().join(format!("ytt-si-test-{}.lock", std::process::id()));
        let a = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: `a` owns a valid fd and flock returns -1/errno instead of UB on
        // contention or platform failure.
        let rc_a = unsafe { libc::flock(a.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc_a, 0, "first holder should acquire");

        // A distinct open file description on the same path must fail non-blocking.
        let b = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: `b` owns a distinct valid fd for the same path; LOCK_NB reports
        // contention via -1/EWOULDBLOCK.
        let rc_b = unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(
            rc_b, -1,
            "second holder must be blocked while the first holds the lock"
        );

        drop(a); // releasing lets the next holder in
        // SAFETY: after dropping `a`, `b` remains a valid fd and can acquire the lock.
        let rc_b2 = unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc_b2, 0, "after release the lock is available");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guard_drop_unlocks_while_a_duplicated_fd_remains_open() {
        let path =
            std::env::temp_dir().join(format!("ytt-si-duplicate-test-{}.lock", std::process::id()));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: `lock` owns a valid fd and flock reports errors through its return value.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let duplicated = lock.try_clone().unwrap();
        let guard = super::InstanceGuard { _lock: lock };
        let contender = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        // SAFETY: `contender` owns a distinct valid fd and nonblocking contention is reported.
        assert_eq!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );

        drop(guard);
        // SAFETY: explicit LOCK_UN in `InstanceGuard::drop` released the shared description.
        assert_eq!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        drop(duplicated);
        // SAFETY: `contender` still owns a valid fd and currently holds the lock.
        let _ = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_UN) };
        drop(contender);
        let _ = std::fs::remove_file(&path);
    }
}
