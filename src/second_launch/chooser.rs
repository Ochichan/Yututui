//! The pre-alternate-screen chooser a second interactive launch renders.
//!
//! Deliberately primitive: bounded plain-text output plus raw-mode single-key reads, no ratatui
//! and no alternate screen. It runs before the art-picker probe and `tui::init`, and the restart
//! path re-enters normal startup afterwards — so it must leave the terminal in exactly the cooked
//! state it found it in (RAII guard below).

use std::io;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::player::lifetime::ShutdownLatch;
use crate::t;
#[cfg(unix)]
use crate::terminal_policy::NORMAL_RESTORE_TIMEOUT;
use crate::terminal_policy::STARTUP_OUTPUT_TIMEOUT;
use crate::tui::PreTuiOutput;

pub const CHOOSER_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_TICK: Duration = Duration::from_millis(250);
#[cfg(unix)]
const CANCELLATION_POLL: Duration = Duration::from_millis(25);
const OUTPUT_LABEL: &str = "second-launch chooser output";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Focus,
    Restart,
    NewInstance,
    Quit,
}

/// Who owns the primary socket. A headless daemon has no terminal to focus, and quitting
/// it kills a background service — the menu must say so instead of pretending it's a TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    Tui,
    Daemon,
}

/// Key → choice, `None` keeps polling. Pure so the mapping is testable without a tty.
pub fn map_key(key: KeyEvent, owner: OwnerKind) -> Option<Choice> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(Choice::Quit),
            _ => None,
        };
    }
    match key.code {
        // With a daemon owner there is nothing to focus; Enter stays the safe default.
        KeyCode::Enter => Some(match owner {
            OwnerKind::Tui => Choice::Focus,
            OwnerKind::Daemon => Choice::Quit,
        }),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Choice::Restart),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(Choice::NewInstance),
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Some(Choice::Quit),
        _ => None,
    }
}

/// Restores cooked mode on every exit path, including panics while a key is pending.
struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        crate::tui::enable_interactive_raw_mode()?;
        Ok(Self { active: true })
    }

    fn restore(mut self) -> io::Result<()> {
        self.active = false;
        restore_raw_mode()
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_raw_mode();
        }
    }
}

fn restore_raw_mode() -> io::Result<()> {
    #[cfg(unix)]
    return crate::tui::bounded_raw_mode_restore(
        "second-launch chooser raw-mode restore",
        NORMAL_RESTORE_TIMEOUT,
    );

    #[cfg(not(unix))]
    crossterm::terminal::disable_raw_mode()
}

/// Render the menu and block for a decision. Timeout, process shutdown, and read errors quit.
///
/// Blocking by design — call from `spawn_blocking`, never on a runtime worker.
pub fn prompt(
    timeout: Duration,
    owner: OwnerKind,
    shutdown: &ShutdownLatch,
) -> std::io::Result<Choice> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "chooser deadline overflow"))?;
    // A signal that won before the blocking prompt started must not open or touch the terminal.
    if shutdown.is_triggered() {
        return Ok(Choice::Quit);
    }
    let output = PreTuiOutput::open_until(deadline, STARTUP_OUTPUT_TIMEOUT)?;

    #[cfg(unix)]
    let cancellation = output.cancellation();
    #[cfg(unix)]
    return with_shutdown_cancellation(
        shutdown,
        move || cancellation.cancel(),
        move || prompt_with_output(output, deadline, owner, shutdown),
    );

    #[cfg(not(unix))]
    prompt_with_output(output, deadline, owner, shutdown)
}

fn prompt_with_output(
    mut output: PreTuiOutput,
    deadline: Instant,
    owner: OwnerKind,
    shutdown: &ShutdownLatch,
) -> io::Result<Choice> {
    output.write_bytes(OUTPUT_LABEL, menu_text(owner).as_bytes())?;
    if shutdown.is_triggered() {
        return Ok(Choice::Quit);
    }
    let raw = RawModeGuard::enable()?;
    let choice: io::Result<Choice> = (|| {
        loop {
            if shutdown.is_triggered() {
                let _ = finish_countdown_line(&mut output);
                return Ok(Choice::Quit);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = print_countdown_line(&mut output, Duration::ZERO);
                let _ = finish_countdown_line(&mut output);
                return Ok(Choice::Quit);
            }
            print_countdown_line(&mut output, remaining)?;
            if crossterm::event::poll(POLL_TICK.min(remaining))? {
                match crossterm::event::read()? {
                    Event::Key(key) if key.is_press() => {
                        if let Some(choice) = map_key(key, owner) {
                            // Terminate the countdown line before cooked-mode output resumes.
                            let _ = finish_countdown_line(&mut output);
                            return Ok(choice);
                        }
                    }
                    _ => {}
                }
            }
        }
    })();
    let restored = raw.restore();
    match (choice, restored) {
        (Ok(choice), Ok(())) => Ok(choice),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; chooser raw-mode restore also failed: {restore_error}"),
        )),
    }
}

/// Write one cooked-mode chooser status without blocking a Tokio runtime worker.
pub async fn write_status(message: String, shutdown: &ShutdownLatch) -> io::Result<()> {
    if shutdown.is_triggered() {
        return Ok(());
    }
    let shutdown = shutdown.clone();
    tokio::task::spawn_blocking(move || write_status_blocking(message, &shutdown))
        .await
        .map_err(|error| io::Error::other(format!("chooser output worker failed: {error}")))?
}

fn write_status_blocking(mut message: String, shutdown: &ShutdownLatch) -> io::Result<()> {
    if shutdown.is_triggered() {
        return Ok(());
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let deadline = Instant::now()
        .checked_add(STARTUP_OUTPUT_TIMEOUT)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "chooser output deadline overflow",
            )
        })?;
    let mut output = PreTuiOutput::open_until(deadline, STARTUP_OUTPUT_TIMEOUT)?;

    #[cfg(unix)]
    let cancellation = output.cancellation();
    #[cfg(unix)]
    return with_shutdown_cancellation(
        shutdown,
        move || cancellation.cancel(),
        move || output.write_bytes(OUTPUT_LABEL, message.as_bytes()),
    );

    #[cfg(not(unix))]
    output.write_bytes(OUTPUT_LABEL, message.as_bytes())
}

#[cfg(unix)]
fn with_shutdown_cancellation<T, C>(
    shutdown: &ShutdownLatch,
    cancel: C,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T>
where
    C: FnOnce() + Send,
{
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let finished_ref = &finished;
        let watcher = scope.spawn(move || {
            while !finished_ref.load(Ordering::Acquire) {
                if shutdown.is_triggered() {
                    cancel();
                    return;
                }
                std::thread::park_timeout(CANCELLATION_POLL);
            }
        });
        let result = operation();
        finished.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    })
}

fn menu_text(owner: OwnerKind) -> String {
    match owner {
        OwnerKind::Tui => format!(
            "{}\n\n{}\n{}\n{}\n",
            t!(
                "YuTuTui! is already running.",
                "YuTuTui!가 이미 실행 중입니다.",
                "YuTuTui!はすでに実行中です。"
            ),
            t!(
                "  [Enter] Switch to the running player",
                "  [Enter] 실행 중인 플레이어로 전환",
                "  [Enter] 実行中のプレイヤーに切り替える"
            ),
            t!(
                "  [r]     Restart here (quit the running player and take over)",
                "  [r]     여기서 다시 시작 (실행 중인 플레이어를 종료하고 인계)",
                "  [r]     ここで再起動 (実行中のプレイヤーを終了して引き継ぐ)"
            ),
            t!(
                "  [n]     Open a second read-only player",
                "  [n]     읽기 전용 두 번째 플레이어 열기",
                "  [n]     2つ目のプレイヤーを読み取り専用で開く"
            )
        ),
        OwnerKind::Daemon => format!(
            "{}\n\n{}\n{}\n",
            t!(
                "YuTuTui! is already running as a background daemon.",
                "YuTuTui!가 이미 백그라운드 데몬으로 실행 중입니다.",
                "YuTuTui!はすでにバックグラウンドデーモンとして実行中です。"
            ),
            t!(
                "  [r]     Stop the background daemon and play here instead",
                "  [r]     백그라운드 데몬을 종료하고 여기서 재생",
                "  [r]     バックグラウンドデーモンを停止して、ここで再生する"
            ),
            t!(
                "  [n]     Open a second read-only player",
                "  [n]     읽기 전용 두 번째 플레이어 열기",
                "  [n]     2つ目のプレイヤーを読み取り専用で開く"
            )
        ),
    }
}

fn print_countdown_line(output: &mut PreTuiOutput, remaining: Duration) -> io::Result<()> {
    let seconds = remaining.as_secs();
    // Raw mode: carriage return only, no cursor addressing. Trailing spaces clear the
    // previous (possibly longer) rendering of the counter.
    let line = format!(
        "\r{}  ",
        t!(
            format!("  [q]     Quit (auto-quits in {seconds}s)"),
            format!("  [q]     종료 ({seconds}초 후 자동 종료)"),
            format!("  [q]     終了 ({seconds}秒後に自動終了)")
        )
    );
    output.write_bytes(OUTPUT_LABEL, line.as_bytes())
}

fn finish_countdown_line(output: &mut PreTuiOutput) -> io::Result<()> {
    output.write_bytes(OUTPUT_LABEL, b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keys_map_to_choices_and_unknown_keys_keep_polling() {
        let tui = OwnerKind::Tui;
        assert_eq!(map_key(key(KeyCode::Enter), tui), Some(Choice::Focus));
        assert_eq!(map_key(key(KeyCode::Char('r')), tui), Some(Choice::Restart));
        assert_eq!(map_key(key(KeyCode::Char('R')), tui), Some(Choice::Restart));
        assert_eq!(
            map_key(key(KeyCode::Char('n')), tui),
            Some(Choice::NewInstance)
        );
        assert_eq!(
            map_key(key(KeyCode::Char('N')), tui),
            Some(Choice::NewInstance)
        );
        assert_eq!(map_key(key(KeyCode::Char('q')), tui), Some(Choice::Quit));
        assert_eq!(map_key(key(KeyCode::Char('Q')), tui), Some(Choice::Quit));
        assert_eq!(map_key(key(KeyCode::Esc), tui), Some(Choice::Quit));
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                tui
            ),
            Some(Choice::Quit)
        );
        assert_eq!(map_key(key(KeyCode::Char('x')), tui), None);
        assert_eq!(map_key(key(KeyCode::Tab), tui), None);
        // A modified letter is not a plain choice key.
        assert_eq!(
            map_key(
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                tui
            ),
            None
        );
    }

    #[test]
    fn daemon_owner_makes_enter_the_safe_quit_never_a_kill() {
        let daemon = OwnerKind::Daemon;
        assert_eq!(map_key(key(KeyCode::Enter), daemon), Some(Choice::Quit));
        // Stopping the daemon stays an explicit, spelled-out keypress.
        assert_eq!(
            map_key(key(KeyCode::Char('r')), daemon),
            Some(Choice::Restart)
        );
        assert_eq!(
            map_key(key(KeyCode::Char('n')), daemon),
            Some(Choice::NewInstance)
        );
    }

    #[test]
    fn shutdown_before_prompt_quits_without_requiring_a_terminal() {
        let shutdown = ShutdownLatch::new();
        shutdown.trigger();

        assert_eq!(
            prompt(CHOOSER_TIMEOUT, OwnerKind::Tui, &shutdown).unwrap(),
            Choice::Quit
        );
    }

    #[tokio::test]
    async fn shutdown_before_status_write_returns_without_requiring_a_terminal() {
        let shutdown = ShutdownLatch::new();
        shutdown.trigger();

        write_status("not written".to_owned(), &shutdown)
            .await
            .unwrap();
    }

    #[test]
    fn menu_is_rendered_as_one_complete_plain_text_write() {
        let tui = menu_text(OwnerKind::Tui);
        assert!(tui.starts_with("YuTuTui!"));
        assert!(tui.contains("[Enter]"));
        assert!(tui.contains("[r]"));
        assert!(tui.contains("[n]"));
        assert!(tui.ends_with('\n'));

        let daemon = menu_text(OwnerKind::Daemon);
        assert!(daemon.contains("background daemon"));
        assert!(!daemon.contains("[Enter]"));
    }
}
