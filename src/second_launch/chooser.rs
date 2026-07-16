//! The pre-alternate-screen chooser a second interactive launch renders.
//!
//! Deliberately primitive: plain `println!` plus raw-mode single-key reads, no ratatui and
//! no alternate screen. It runs before the art-picker probe and `tui::init`, and the
//! restart path re-enters normal startup afterwards — so it must leave the terminal in
//! exactly the cooked state it found it in (RAII guard below).

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::t;

pub const CHOOSER_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_TICK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Focus,
    Restart,
    NewInstance,
    Quit,
}

/// Key → choice, `None` keeps polling. Pure so the mapping is testable without a tty.
pub fn map_key(key: KeyEvent) -> Option<Choice> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(Choice::Quit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Enter => Some(Choice::Focus),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Choice::Restart),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(Choice::NewInstance),
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Some(Choice::Quit),
        _ => None,
    }
}

/// Restores cooked mode on every exit path, including panics while a key is pending.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Render the menu and block for a decision. Timeout and read errors quit.
///
/// Blocking by design — call from `spawn_blocking`, never on a runtime worker.
pub fn prompt(timeout: Duration) -> std::io::Result<Choice> {
    println!(
        "{}",
        t!(
            "YuTuTui! is already running.",
            "YuTuTui!가 이미 실행 중입니다."
        )
    );
    println!();
    println!(
        "{}",
        t!(
            "  [Enter] Switch to the running player",
            "  [Enter] 실행 중인 플레이어로 전환"
        )
    );
    println!(
        "{}",
        t!(
            "  [r]     Restart here (quit the running player and take over)",
            "  [r]     여기서 다시 시작 (실행 중인 플레이어를 종료하고 인계)"
        )
    );
    println!(
        "{}",
        t!(
            "  [n]     Open a second read-only player",
            "  [n]     읽기 전용 두 번째 플레이어 열기"
        )
    );
    let deadline = Instant::now() + timeout;
    let _raw = RawModeGuard::enable()?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            print_countdown_line(Duration::ZERO);
            println!("\r");
            return Ok(Choice::Quit);
        }
        print_countdown_line(remaining);
        if crossterm::event::poll(POLL_TICK.min(remaining))? {
            match crossterm::event::read()? {
                Event::Key(key) if key.is_press() => {
                    if let Some(choice) = map_key(key) {
                        // Terminate the countdown line before cooked-mode output resumes.
                        println!("\r");
                        return Ok(choice);
                    }
                }
                _ => {}
            }
        }
    }
}

fn print_countdown_line(remaining: Duration) {
    let seconds = remaining.as_secs();
    // Raw mode: carriage return only, no cursor addressing. Trailing spaces clear the
    // previous (possibly longer) rendering of the counter.
    print!(
        "\r{}  ",
        t!(
            format!("  [q]     Quit (auto-quits in {seconds}s)"),
            format!("  [q]     종료 ({seconds}초 후 자동 종료)")
        )
    );
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keys_map_to_choices_and_unknown_keys_keep_polling() {
        assert_eq!(map_key(key(KeyCode::Enter)), Some(Choice::Focus));
        assert_eq!(map_key(key(KeyCode::Char('r'))), Some(Choice::Restart));
        assert_eq!(map_key(key(KeyCode::Char('R'))), Some(Choice::Restart));
        assert_eq!(map_key(key(KeyCode::Char('n'))), Some(Choice::NewInstance));
        assert_eq!(map_key(key(KeyCode::Char('N'))), Some(Choice::NewInstance));
        assert_eq!(map_key(key(KeyCode::Char('q'))), Some(Choice::Quit));
        assert_eq!(map_key(key(KeyCode::Char('Q'))), Some(Choice::Quit));
        assert_eq!(map_key(key(KeyCode::Esc)), Some(Choice::Quit));
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Choice::Quit)
        );
        assert_eq!(map_key(key(KeyCode::Char('x'))), None);
        assert_eq!(map_key(key(KeyCode::Tab)), None);
        // A modified letter is not a plain choice key.
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
    }
}
