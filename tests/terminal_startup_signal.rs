#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;
use rustix::fs::{CWD, Mode, OFlags, fcntl_getfl, fcntl_setfl, openat};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};
use yututui::terminal_runtime::{
    InteractiveSignals, STARTUP_OUTPUT_TIMEOUT, build_art_picker_with_access_until_bounded,
};
use yututui::{tui, zoom::ZoomHandle};

const CHILD_MARKER: &str = "YUTUTUI_TEST_STARTUP_SIGNAL_PTY_CHILD";
const SUPERVISOR_MARKER: &str = "YUTUTUI_TEST_PTY_SESSION_SUPERVISOR";
const SUPERVISOR_STATE_DIR: &str = "YUTUTUI_TEST_PTY_SUPERVISOR_STATE_DIR";
const SUPERVISOR_TARGET_TEST: &str = "YUTUTUI_TEST_PTY_SUPERVISOR_TARGET";
const SUPERVISOR_TEST_NAME: &str = "pty_session_supervisor";
const CHILD_TEST_NAME: &str = "startup_signal_pty_child";
const REPEATED_SIGNAL_CHILD_TEST_NAME: &str = "repeated_signal_art_query_pty_child";
const TERMINAL_DROP_CHILD_TEST_NAME: &str = "terminal_drop_pty_child";
const TERMINAL_PANIC_CHILD_TEST_NAME: &str = "terminal_panic_pty_child";
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const ART_CELL_SIZE_QUERY: &[u8] = b"\x1b[16t";
const FIRST_SIGNAL_OBSERVED: &[u8] = b"YUTUTUI_TEST_FIRST_SIGTERM_OBSERVED";
const PANIC_FIXTURE_MARKER: &[u8] = b"YUTUTUI_TEST_TERMINAL_PANIC";
const RATATUI_CURSOR_DROP_ERROR: &[u8] = b"Failed to show the cursor";
const UNSCOPED_OUTPUT_ERROR: &[u8] = b"terminal output write attempted without an active operation";
const ENTER_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_AFTER_SIGNAL_TIMEOUT: Duration = Duration::from_secs(6);
const FIRST_SIGNAL_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(2);
const REPEATED_SIGNAL_DELAY: Duration = Duration::from_millis(50);
const HARD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const SUPERVISOR_RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_SUPERVISOR_ID: AtomicUsize = AtomicUsize::new(0);

struct KillTargetOnDrop {
    child: Child,
}

impl KillTargetOnDrop {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for KillTargetOnDrop {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct KillChildOnDrop {
    supervisor: Child,
    state_dir: PathBuf,
    signal_requests: u32,
    released_and_reaped: bool,
}

impl KillChildOnDrop {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match fs::read_to_string(self.state_dir.join("target-status")) {
            Ok(raw) => {
                let raw = raw.trim().parse::<i32>().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid PTY target status {raw:?}: {error}"),
                    )
                })?;
                return Ok(Some(ExitStatus::from_raw(raw)));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        if let Some(status) = self.supervisor.try_wait()? {
            return Err(io::Error::other(format!(
                "PTY session supervisor exited before publishing target status: {status:?}"
            )));
        }
        Ok(None)
    }

    fn wait_for_target_ready(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + ENTER_TIMEOUT;
        loop {
            match fs::metadata(self.state_dir.join("target-ready")) {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }

            if let Some(status) = self.supervisor.try_wait()? {
                return Err(io::Error::other(format!(
                    "PTY session supervisor exited before publishing target readiness: {status:?}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY session supervisor did not publish target readiness in time",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn send_sigterm(&mut self) -> io::Result<()> {
        self.wait_for_target_ready()?;
        if let Some(status) = self.try_wait()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("PTY target exited before SIGTERM could be requested: {status:?}"),
            ));
        }

        self.signal_requests = self
            .signal_requests
            .checked_add(1)
            .ok_or_else(|| io::Error::other("PTY signal request counter overflowed"))?;
        publish_supervisor_state(
            &self.state_dir.join("signal-request"),
            self.signal_requests.to_string().as_bytes(),
        )?;

        let deadline = Instant::now() + ENTER_TIMEOUT;
        loop {
            if read_supervisor_counter(&self.state_dir.join("signal-ack"))?
                .is_some_and(|acknowledged| acknowledged >= self.signal_requests)
            {
                return Ok(());
            }
            if let Some(status) = self.try_wait()? {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("PTY target exited before acknowledging SIGTERM: {status:?}"),
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY session supervisor did not acknowledge SIGTERM in time",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn release_and_wait(&mut self, master: &OwnedFd) -> io::Result<()> {
        fs::write(self.state_dir.join("release"), b"release")?;
        let deadline = Instant::now() + ENTER_TIMEOUT;
        let mut trailing_output = Vec::new();
        loop {
            if let Some(status) = self.supervisor.try_wait()? {
                self.released_and_reaped = true;
                if status.success() {
                    return Ok(());
                }
                return Err(io::Error::other(format!(
                    "PTY session supervisor failed after release: {status:?}"
                )));
            }
            // On XNU a controlling-session leader waits for the PTY output queue to drain before
            // exit/revoke. Keep reading the libtest suffix after release so that wait stays bounded.
            drain_master(master, &mut trailing_output)?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY session supervisor did not exit after release",
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for KillChildOnDrop {
    fn drop(&mut self) {
        if !self.released_and_reaped {
            match self.supervisor.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    if let Ok(process_group) = libc::pid_t::try_from(self.supervisor.id()) {
                        // SAFETY: the unreaped supervisor called `setsid`, so its positive pid is
                        // still reserved as the process group id for only this isolated fixture.
                        // Killing the negative id cleans up it and any unfinished target.
                        let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
                    } else {
                        let _ = self.supervisor.kill();
                    }
                    let _ = self.supervisor.wait();
                }
            }
        }
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

fn create_supervisor_state_dir() -> io::Result<PathBuf> {
    for _ in 0..1024 {
        let id = NEXT_SUPERVISOR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-pty-supervisor-{}-{id}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique PTY supervisor state directory",
    ))
}

fn publish_supervisor_state(path: &Path, value: impl AsRef<[u8]>) -> io::Result<()> {
    let pending = path.with_extension("pending");
    fs::write(&pending, value)?;
    fs::rename(pending, path)
}

fn read_supervisor_counter(path: &Path) -> io::Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(value) => value.trim().parse::<u32>().map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid PTY supervisor counter {value:?}: {error}"),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn pty_pair() -> io::Result<(OwnedFd, File)> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(io::Error::from)?;
    grantpt(&master).map_err(io::Error::from)?;
    unlockpt(&master).map_err(io::Error::from)?;

    let path = ptsname(&master, Vec::new()).map_err(io::Error::from)?;
    let slave = openat(
        CWD,
        path.as_c_str(),
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .map_err(io::Error::from)?;

    let flags = fcntl_getfl(&master).map_err(io::Error::from)?;
    fcntl_setfl(&master, flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
    Ok((master, File::from(slave)))
}

fn spawn_pty_child_with(
    slave: &File,
    child_test_name: &str,
    configure: impl FnOnce(&mut Command),
) -> io::Result<KillChildOnDrop> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--ignored",
            "--exact",
            SUPERVISOR_TEST_NAME,
            "--test-threads=1",
            "--nocapture",
        ])
        .env(SUPERVISOR_MARKER, "1")
        .env(SUPERVISOR_TARGET_TEST, child_test_name)
        .env(CHILD_MARKER, "1")
        // Keep keyboard negotiation deterministic, then deliberately force the zoom CPR probe.
        // With no synthetic CPR reply, SIGTERM is guaranteed to land inside `init_until` rather
        // than racing an initialization path that finishes between two parent polls.
        .env("TERM", "dumb")
        .env("YTM_TUI_KEYBOARD_ENHANCEMENT", "off")
        .env("YTM_TUI_WIN32_INPUT", "off")
        .env("YTM_TUI_TEXT_SIZING", "on")
        .env_remove("KITTY_WINDOW_ID")
        .env_remove("KONSOLE_VERSION")
        .env_remove("TERM_PROGRAM")
        .env_remove("WEZTERM_EXECUTABLE")
        .env_remove("WT_SESSION")
        .env_remove("TMUX")
        .env_remove("YTM_TUI_IMAGE_PROTOCOL")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    configure(&mut command);

    let state_dir = create_supervisor_state_dir()?;
    command.env(SUPERVISOR_STATE_DIR, &state_dir);

    // SAFETY: after `fork` this closure invokes only `setsid`, `ioctl`, and errno capture before
    // `exec`. File-descriptor redirection has already made stdin the PTY slave. The supervisor's
    // new session and TIOCSCTTY make that slave `/dev/tty`; the target inherits both while the
    // terminal running the parent test remains untouched.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // libc's ioctl request type varies across Unix targets.
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    match command.spawn() {
        Ok(supervisor) => Ok(KillChildOnDrop {
            supervisor,
            state_dir,
            signal_requests: 0,
            released_and_reaped: false,
        }),
        Err(error) => {
            let _ = fs::remove_dir(&state_dir);
            Err(error)
        }
    }
}

fn spawn_pty_child(slave: &File) -> io::Result<KillChildOnDrop> {
    spawn_pty_child_with(slave, CHILD_TEST_NAME, |_| {})
}

fn spawn_repeated_signal_art_query_child(slave: &File) -> io::Result<KillChildOnDrop> {
    let isolated_root = std::env::temp_dir().join(format!(
        "yututui-terminal-signal-test-{}",
        std::process::id()
    ));
    spawn_pty_child_with(slave, REPEATED_SIGNAL_CHILD_TEST_NAME, |command| {
        command
            // A native-image hint bypasses the halfblocks cache and guarantees a real query.
            .env("TERM", "xterm-kitty")
            .env("KITTY_WINDOW_ID", "1")
            // Every path is synthetic and the child uses a read-only persistence capability.
            .env("HOME", &isolated_root)
            .env("XDG_CONFIG_HOME", isolated_root.join("config"))
            .env("XDG_DATA_HOME", isolated_root.join("data"))
            .env("XDG_CACHE_HOME", isolated_root.join("cache"))
            .env("XDG_STATE_HOME", isolated_root.join("state"))
            .env("XDG_RUNTIME_DIR", isolated_root.join("runtime"))
            .env("YTM_CONFIG_DIR", isolated_root.join("config"))
            .env("YTM_DATA_DIR", isolated_root.join("data"))
            .env("YTM_CACHE_DIR", isolated_root.join("cache"));
    })
}

fn spawn_terminal_teardown_child(
    slave: &File,
    child_test_name: &str,
) -> io::Result<KillChildOnDrop> {
    spawn_pty_child_with(slave, child_test_name, |command| {
        command.env("YTM_TUI_TEXT_SIZING", "off");
    })
}

fn drain_master(master: &OwnedFd, transcript: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        match rustix::io::read(master, &mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => transcript.extend_from_slice(&chunk[..read]),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn escaped_transcript(transcript: &[u8]) -> String {
    String::from_utf8_lossy(transcript)
        .escape_debug()
        .to_string()
}

fn wait_for_child_exit(
    child: &mut KillChildOnDrop,
    master: &OwnedFd,
    timeout: Duration,
    label: &str,
) -> (ExitStatus, Vec<u8>) {
    let deadline = Instant::now() + timeout;
    let mut transcript = Vec::new();
    loop {
        drain_master(master, &mut transcript).expect("PTY master should remain readable");
        if let Some(status) = child
            .try_wait()
            .expect("PTY child status should be readable")
        {
            drain_master(master, &mut transcript).expect("final PTY output should be readable");
            return (status, transcript);
        }
        assert!(
            Instant::now() < deadline,
            "{label} exceeded its bounded exit; transcript={}",
            escaped_transcript(&transcript)
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn assert_app_terminal_restore_sequence(transcript: &[u8]) {
    let entered = find_bytes(transcript, ENTER_ALTERNATE_SCREEN).unwrap_or_else(|| {
        panic!(
            "child did not enter the alternate screen; transcript={}",
            escaped_transcript(transcript)
        )
    });
    let hidden = find_bytes(transcript, HIDE_CURSOR).unwrap_or_else(|| {
        panic!(
            "child did not render a cursorless frame; transcript={}",
            escaped_transcript(transcript)
        )
    });
    let shown = find_bytes(transcript, SHOW_CURSOR).unwrap_or_else(|| {
        panic!(
            "bounded restore did not show the cursor; transcript={}",
            escaped_transcript(transcript)
        )
    });
    let left = find_bytes(transcript, LEAVE_ALTERNATE_SCREEN).unwrap_or_else(|| {
        panic!(
            "bounded restore did not leave the alternate screen; transcript={}",
            escaped_transcript(transcript)
        )
    });
    assert!(
        entered < hidden && hidden < shown && shown < left,
        "terminal activation/render/restore order was invalid; transcript={}",
        escaped_transcript(transcript)
    );
    assert_eq!(
        transcript
            .windows(SHOW_CURSOR.len())
            .filter(|bytes| *bytes == SHOW_CURSOR)
            .count(),
        1,
        "only the bounded restore owner may physically show the cursor; transcript={}",
        escaped_transcript(transcript)
    );
    for forbidden in [RATATUI_CURSOR_DROP_ERROR, UNSCOPED_OUTPUT_ERROR] {
        assert!(
            find_bytes(transcript, forbidden).is_none(),
            "Ratatui destructor bypassed bounded terminal output; transcript={}",
            escaped_transcript(transcript)
        );
    }
}

fn assert_termios_restored(before: &rustix::termios::Termios, after: &rustix::termios::Termios) {
    assert!(
        after
            .local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO),
        "child left the PTY raw or non-echoing: before={:?}, after={:?}",
        before.local_modes,
        after.local_modes
    );
    assert_eq!(after.input_modes, before.input_modes);
    assert_eq!(after.output_modes, before.output_modes);
    assert_eq!(after.control_modes, before.control_modes);
    assert_eq!(after.local_modes, before.local_modes);
}

#[test]
fn app_terminal_drop_defers_physical_cursor_restore_to_bounded_owner() {
    let (master, slave) = pty_pair().expect("isolated PTY should be available");
    let before = tcgetattr(&slave).expect("PTY termios should be readable before the child starts");
    let mut child = spawn_terminal_teardown_child(&slave, TERMINAL_DROP_CHILD_TEST_NAME)
        .expect("terminal-drop PTY child should start");
    let (status, transcript) = wait_for_child_exit(
        &mut child,
        &master,
        ENTER_TIMEOUT,
        "terminal-drop PTY child",
    );

    assert!(
        status.success(),
        "terminal-drop child failed: status={status:?}; transcript={}",
        escaped_transcript(&transcript)
    );
    assert_app_terminal_restore_sequence(&transcript);
    let after = tcgetattr(&slave).expect("PTY termios should remain readable after child exit");
    assert_termios_restored(&before, &after);
    child
        .release_and_wait(&master)
        .expect("PTY session supervisor should exit after restoration is checked");
}

#[test]
fn panic_unwind_does_not_reenter_unbounded_ratatui_cursor_output() {
    let (master, slave) = pty_pair().expect("isolated PTY should be available");
    let before = tcgetattr(&slave).expect("PTY termios should be readable before the child starts");
    let mut child = spawn_terminal_teardown_child(&slave, TERMINAL_PANIC_CHILD_TEST_NAME)
        .expect("terminal-panic PTY child should start");
    let (status, transcript) = wait_for_child_exit(
        &mut child,
        &master,
        ENTER_TIMEOUT,
        "terminal-panic PTY child",
    );

    assert!(
        !status.success(),
        "intentional panic fixture unexpectedly succeeded; transcript={}",
        escaped_transcript(&transcript)
    );
    assert!(
        find_bytes(&transcript, PANIC_FIXTURE_MARKER).is_some(),
        "child did not reach its intentional panic; transcript={}",
        escaped_transcript(&transcript)
    );
    assert_app_terminal_restore_sequence(&transcript);
    let after = tcgetattr(&slave).expect("PTY termios should remain readable after child exit");
    assert_termios_restored(&before, &after);
    child
        .release_and_wait(&master)
        .expect("PTY session supervisor should exit after restoration is checked");
}

#[test]
fn sigterm_during_terminal_init_restores_the_controlling_pty() {
    let (master, slave) = pty_pair().expect("isolated PTY should be available");
    let before = tcgetattr(&slave).expect("PTY termios should be readable before the child starts");
    assert!(
        before
            .local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO),
        "a new PTY should begin in canonical, echoing mode"
    );

    let mut child = spawn_pty_child(&slave).expect("PTY test child should start");
    let started = Instant::now();
    let mut signal_sent_at = None;
    let mut transcript = Vec::new();
    let status = loop {
        drain_master(&master, &mut transcript).expect("PTY master should remain readable");

        if signal_sent_at.is_none() && find_bytes(&transcript, ENTER_ALTERNATE_SCREEN).is_some() {
            child
                .send_sigterm()
                .expect("SIGTERM should reach the live PTY child");
            signal_sent_at = Some(Instant::now());
        }

        if let Some(status) = child
            .try_wait()
            .expect("PTY child status should be readable")
        {
            drain_master(&master, &mut transcript).expect("final PTY output should be readable");
            break status;
        }

        let deadline = signal_sent_at
            .map(|sent| sent + EXIT_AFTER_SIGNAL_TIMEOUT)
            .unwrap_or(started + ENTER_TIMEOUT);
        assert!(
            Instant::now() < deadline,
            "PTY child exceeded its bounded startup/signal exit; transcript={}",
            escaped_transcript(&transcript)
        );
        thread::sleep(Duration::from_millis(2));
    };

    let sent_at = signal_sent_at.unwrap_or_else(|| {
        panic!(
            "child exited before entering the alternate screen; status={status:?}; transcript={}",
            escaped_transcript(&transcript)
        )
    });
    assert!(
        sent_at.elapsed() < EXIT_AFTER_SIGNAL_TIMEOUT,
        "SIGTERM shutdown exceeded its bound"
    );

    let entered = find_bytes(&transcript, ENTER_ALTERNATE_SCREEN)
        .expect("child should enter the alternate screen before SIGTERM");
    assert!(
        find_bytes(&transcript, CURSOR_POSITION_QUERY).is_some(),
        "forced CPR should keep the child inside terminal initialization; transcript={}",
        escaped_transcript(&transcript)
    );
    let left = find_bytes(&transcript, LEAVE_ALTERNATE_SCREEN).unwrap_or_else(|| {
        panic!(
            "child did not leave the alternate screen after SIGTERM; transcript={}",
            escaped_transcript(&transcript)
        )
    });
    assert!(
        left > entered,
        "alternate-screen restore preceded activation"
    );

    let after = tcgetattr(&slave).expect("PTY termios should remain readable after child exit");
    assert!(
        after
            .local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO),
        "SIGTERM left the PTY raw or non-echoing: before={:?}, after={:?}",
        before.local_modes,
        after.local_modes
    );
    assert_eq!(after.input_modes, before.input_modes);
    assert_eq!(after.output_modes, before.output_modes);
    assert_eq!(after.control_modes, before.control_modes);
    assert_eq!(after.local_modes, before.local_modes);
    assert!(
        status.success(),
        "PTY child did not complete cooperative SIGTERM shutdown: status={status:?}; transcript={}",
        escaped_transcript(&transcript)
    );
    child
        .release_and_wait(&master)
        .expect("PTY session supervisor should exit after restoration is checked");
}

#[test]
fn repeated_sigterm_during_art_query_forces_bounded_exit_and_restores_the_pty() {
    let (master, slave) = pty_pair().expect("isolated PTY should be available");
    let before = tcgetattr(&slave).expect("PTY termios should be readable before the child starts");
    assert!(
        before
            .local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO),
        "a new PTY should begin in canonical, echoing mode"
    );

    let mut child =
        spawn_repeated_signal_art_query_child(&slave).expect("PTY test child should start");
    let started = Instant::now();
    let mut first_signal_sent_at = None;
    let mut first_signal_observed_at = None;
    let mut second_signal_sent_at = None;
    let mut transcript = Vec::new();
    let status = loop {
        drain_master(&master, &mut transcript).expect("PTY master should remain readable");

        if first_signal_sent_at.is_none() && find_bytes(&transcript, ART_CELL_SIZE_QUERY).is_some()
        {
            child
                .send_sigterm()
                .expect("first SIGTERM should reach the live PTY child");
            first_signal_sent_at = Some(Instant::now());
        }

        if first_signal_observed_at.is_none()
            && find_bytes(&transcript, FIRST_SIGNAL_OBSERVED).is_some()
        {
            first_signal_observed_at = Some(Instant::now());
        }

        if second_signal_sent_at.is_none()
            && let (Some(first_sent), Some(_observed)) =
                (first_signal_sent_at, first_signal_observed_at)
            && first_sent.elapsed() >= REPEATED_SIGNAL_DELAY
        {
            child
                .send_sigterm()
                .expect("second SIGTERM should reach the live PTY child");
            second_signal_sent_at = Some(Instant::now());
        }

        if let Some(status) = child
            .try_wait()
            .expect("PTY child status should be readable")
        {
            drain_master(&master, &mut transcript).expect("final PTY output should be readable");
            break status;
        }

        let deadline = if let Some(second_sent) = second_signal_sent_at {
            second_sent + HARD_EXIT_TIMEOUT
        } else if let Some(first_sent) = first_signal_sent_at {
            first_sent + FIRST_SIGNAL_OBSERVATION_TIMEOUT
        } else {
            started + ENTER_TIMEOUT
        };
        assert!(
            Instant::now() < deadline,
            "repeated-SIGTERM PTY child exceeded its phase deadline; transcript={}",
            escaped_transcript(&transcript)
        );
        thread::sleep(Duration::from_millis(2));
    };

    let first_sent = first_signal_sent_at.unwrap_or_else(|| {
        panic!(
            "child exited before writing an art capability query; status={status:?}; transcript={}",
            escaped_transcript(&transcript)
        )
    });
    let first_observed = first_signal_observed_at.unwrap_or_else(|| {
        panic!(
            "child never observed the first SIGTERM; status={status:?}; transcript={}",
            escaped_transcript(&transcript)
        )
    });
    assert!(
        first_observed.saturating_duration_since(first_sent) < FIRST_SIGNAL_OBSERVATION_TIMEOUT,
        "first SIGTERM was not observed promptly"
    );
    let second_sent = second_signal_sent_at.unwrap_or_else(|| {
        panic!(
            "child exited before the repeated SIGTERM; status={status:?}; transcript={}",
            escaped_transcript(&transcript)
        )
    });
    let repeated_after = second_sent.saturating_duration_since(first_sent);
    assert!(
        (REPEATED_SIGNAL_DELAY..FIRST_SIGNAL_OBSERVATION_TIMEOUT).contains(&repeated_after),
        "repeated SIGTERM interval was outside the short bounded window: {repeated_after:?}"
    );
    assert!(
        second_sent.elapsed() < HARD_EXIT_TIMEOUT,
        "second SIGTERM hard exit exceeded its bound"
    );
    assert_eq!(
        status.code(),
        Some(143),
        "repeated SIGTERM must use the shell-convention hard-exit code; status={status:?}; transcript={}",
        escaped_transcript(&transcript)
    );

    if let Some(entered) = find_bytes(&transcript, ENTER_ALTERNATE_SCREEN) {
        let left = find_bytes(&transcript, LEAVE_ALTERNATE_SCREEN).unwrap_or_else(|| {
            panic!(
                "child entered but did not leave the alternate screen; transcript={}",
                escaped_transcript(&transcript)
            )
        });
        assert!(
            left > entered,
            "alternate-screen restore preceded activation"
        );
    }

    let after = tcgetattr(&slave).expect("PTY termios should remain readable after hard exit");
    assert!(
        after
            .local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO),
        "repeated SIGTERM left the PTY raw or non-echoing: before={:?}, after={:?}",
        before.local_modes,
        after.local_modes
    );
    assert_eq!(after.input_modes, before.input_modes);
    assert_eq!(after.output_modes, before.output_modes);
    assert_eq!(after.control_modes, before.control_modes);
    assert_eq!(after.local_modes, before.local_modes);
    child
        .release_and_wait(&master)
        .expect("PTY session supervisor should exit after restoration is checked");
}

/// Own the controlling PTY while the actual fixture runs as an ordinary child of this session.
///
/// XNU revokes a controlling terminal when its session leader exits, invalidating even the
/// parent test's already-open slave descriptor. Keeping this process alive until the parent has
/// compared termios mirrors a real shell-owned terminal and preserves the post-exit restoration
/// assertion on macOS without weakening it to accept `ENOTTY`.
#[test]
#[ignore = "subprocess fixture; run through the parent PTY lifecycle tests"]
fn pty_session_supervisor() {
    if std::env::var_os(SUPERVISOR_MARKER).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }

    let state_dir = PathBuf::from(
        std::env::var_os(SUPERVISOR_STATE_DIR)
            .expect("PTY supervisor state directory should be configured"),
    );
    let target_test = std::env::var(SUPERVISOR_TARGET_TEST)
        .expect("PTY supervisor target test should be configured");
    let mut target = KillTargetOnDrop {
        child: Command::new(std::env::current_exe().expect("test executable should resolve"))
            .args([
                "--ignored",
                "--exact",
                &target_test,
                "--test-threads=1",
                "--nocapture",
            ])
            .env_remove(SUPERVISOR_MARKER)
            .env_remove(SUPERVISOR_STATE_DIR)
            .env_remove(SUPERVISOR_TARGET_TEST)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("PTY supervisor target should start"),
    };

    publish_supervisor_state(&state_dir.join("target-ready"), b"ready")
        .expect("PTY supervisor should publish target readiness");
    let mut forwarded_signals = 0_u32;
    let status = 'target: loop {
        if let Some(status) = target
            .try_wait()
            .expect("PTY supervisor target status should be readable")
        {
            break status;
        }

        let requested_signals = read_supervisor_counter(&state_dir.join("signal-request"))
            .expect("PTY supervisor signal request should be readable")
            .unwrap_or(forwarded_signals);
        assert!(
            requested_signals >= forwarded_signals,
            "PTY signal request counter moved backwards"
        );
        while forwarded_signals < requested_signals {
            if let Some(status) = target
                .try_wait()
                .expect("PTY supervisor target status should be readable before SIGTERM")
            {
                break 'target status;
            }
            let pid = libc::pid_t::try_from(target.id())
                .expect("PTY supervisor target pid should fit pid_t");
            // SAFETY: `try_wait` just proved that this exact owned Child remains unreaped, so its
            // pid cannot be reused between the check and `kill`; `kill` dereferences no memory.
            if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
                let error = io::Error::last_os_error();
                if let Some(status) = target
                    .try_wait()
                    .expect("PTY supervisor target status should remain readable after kill")
                {
                    break 'target status;
                }
                panic!("PTY supervisor could not forward SIGTERM: {error}");
            }
            forwarded_signals += 1;
            publish_supervisor_state(
                &state_dir.join("signal-ack"),
                forwarded_signals.to_string().as_bytes(),
            )
            .expect("PTY supervisor should acknowledge forwarded SIGTERM");
        }
        thread::sleep(Duration::from_millis(2));
    };
    publish_supervisor_state(
        &state_dir.join("target-status"),
        status.into_raw().to_string().as_bytes(),
    )
    .expect("PTY supervisor should publish target status");

    let deadline = Instant::now() + SUPERVISOR_RELEASE_TIMEOUT;
    while !state_dir.join("release").exists() {
        assert!(
            Instant::now() < deadline,
            "PTY parent did not release the session supervisor after checking termios"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

/// Invoked only by [`sigterm_during_terminal_init_restores_the_controlling_pty`]. The marker makes
/// `cargo test -- --ignored` harmless on a developer's real terminal.
#[test]
#[ignore = "subprocess fixture; run through sigterm_during_terminal_init_restores_the_controlling_pty"]
fn startup_signal_pty_child() {
    if std::env::var_os(CHILD_MARKER).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime should start for the signal fixture");
    runtime.block_on(async {
        // Registration intentionally precedes every raw-mode and alternate-screen operation.
        let signals = InteractiveSignals::install().expect("signal streams should install");
        signals.set_mouse(false);

        let initialized = tui::init_until(
            false,
            ZoomHandle::default(),
            Instant::now() + STARTUP_OUTPUT_TIMEOUT,
        );
        let initialization_error = initialized
            .as_ref()
            .err()
            .map(std::string::ToString::to_string);
        let terminal = initialized.ok().map(|(terminal, _keyboard_mode)| terminal);

        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            while !signals.shutdown_requested() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .is_ok();

        // This is the startup caller's ordinary cooperative teardown path. It is safe whether
        // initialization succeeded or already performed its own bounded error restoration.
        drop(terminal);
        tui::restore(false).expect("cooperative signal teardown should restore the PTY");
        signals.shutdown().await;
        assert!(
            observed,
            "SIGTERM did not reach the pre-installed shutdown latch; terminal init error={initialization_error:?}"
        );
    });
}

/// Repeated-signal fixture: issue a real pre-TUI art capability query, report that the first
/// signal reached the latch, then deliberately leave startup wedged so the second signal must use
/// the hard-exit path. The marker keeps direct `--ignored` runs inert on a developer terminal.
#[test]
#[ignore = "subprocess fixture; run through repeated_sigterm_during_art_query_forces_bounded_exit_and_restores_the_pty"]
fn repeated_signal_art_query_pty_child() {
    if std::env::var_os(CHILD_MARKER).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Tokio runtime should start for the repeated-signal fixture");
    runtime.block_on(async {
        let signals = InteractiveSignals::install().expect("signal streams should install");
        signals.set_mouse(false);

        let observed_shutdown = signals.shutdown_latch();
        let observer = tokio::spawn(async move {
            observed_shutdown.wait().await;
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(FIRST_SIGNAL_OBSERVED)
                .and_then(|_| stdout.flush())
                .expect("first-signal marker should reach the PTY parent");
        });

        let persistence_access = yututui::persist::initialize_persistence_reader()
            .expect("art-query fixture should establish read-only persistence");
        let _ = build_art_picker_with_access_until_bounded(
            &persistence_access,
            Instant::now() + STARTUP_OUTPUT_TIMEOUT,
            signals.shutdown_latch(),
            signals.query_cancellation(),
        )
        .await;
        observer
            .await
            .expect("first-signal observer task should complete");

        // Model a startup owner wedged after the first cooperative request. Keeping `signals`
        // alive is essential: the second SIGTERM must reach the escalation phase and exit(143).
        let _signals = signals;
        std::future::pending::<()>().await;
    });
}

/// Normal teardown fixture for [`app_terminal_drop_defers_physical_cursor_restore_to_bounded_owner`].
#[test]
#[ignore = "subprocess fixture; run through app_terminal_drop_defers_physical_cursor_restore_to_bounded_owner"]
fn terminal_drop_pty_child() {
    if std::env::var_os(CHILD_MARKER).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }

    let (mut terminal, _keyboard_mode) = tui::init_until(
        false,
        ZoomHandle::default(),
        Instant::now() + STARTUP_OUTPUT_TIMEOUT,
    )
    .expect("terminal-drop fixture should initialize its PTY");
    tui::draw_frame(&mut terminal, false, false, |_| {})
        .expect("terminal-drop fixture should render one cursorless frame");
    drop(terminal);
    tui::restore(false).expect("terminal-drop fixture should restore its PTY");
}

/// Panic teardown fixture for [`panic_unwind_does_not_reenter_unbounded_ratatui_cursor_output`].
#[test]
#[ignore = "subprocess fixture; run through panic_unwind_does_not_reenter_unbounded_ratatui_cursor_output"]
fn terminal_panic_pty_child() {
    if std::env::var_os(CHILD_MARKER).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }

    let (mut terminal, _keyboard_mode) = tui::init_until(
        false,
        ZoomHandle::default(),
        Instant::now() + STARTUP_OUTPUT_TIMEOUT,
    )
    .expect("terminal-panic fixture should initialize its PTY");
    tui::draw_frame(&mut terminal, false, false, |_| {})
        .expect("terminal-panic fixture should render one cursorless frame");
    panic!("{}", String::from_utf8_lossy(PANIC_FIXTURE_MARKER));
}
