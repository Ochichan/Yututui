//! Opening URLs in the system browser, shared by the TUI (About card, account
//! connect flows) and one-shot CLI commands (`ytt auth …`).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::util::process;

const EARLY_EXIT_WINDOW: Duration = Duration::from_millis(700);
const STDERR_TAIL_MAX: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserOpenReport {
    pub status: BrowserOpenStatus,
    pub attempts: Vec<BrowserOpenAttempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserOpenStatus {
    /// The opener exited successfully or stayed alive long enough to imply handoff.
    Launched,
    /// Every opener failed before the handoff window elapsed.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserOpenAttempt {
    pub opener: String,
    pub outcome: BrowserOpenOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserOpenOutcome {
    SpawnFailed(String),
    Exited { code: Option<i32>, stderr: String },
    StillRunning,
}

impl BrowserOpenReport {
    pub fn launched(&self) -> bool {
        self.status == BrowserOpenStatus::Launched
    }

    pub fn opener(&self) -> Option<&str> {
        self.attempts.last().map(|a| a.opener.as_str())
    }

    pub fn failure_summary(&self) -> String {
        self.attempts
            .iter()
            .map(|attempt| {
                let outcome = match &attempt.outcome {
                    BrowserOpenOutcome::SpawnFailed(e) => format!("spawn failed: {e}"),
                    BrowserOpenOutcome::Exited { code, stderr } => {
                        let code = code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".to_string());
                        if stderr.trim().is_empty() {
                            format!("exited with {code}")
                        } else {
                            format!("exited with {code}: {}", stderr.trim())
                        }
                    }
                    BrowserOpenOutcome::StillRunning => "still running".to_string(),
                };
                format!("{} ({outcome})", attempt.opener)
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Open `url` in the system's default browser, fire-and-forget. Spawns the platform opener
/// (`open` / `xdg-open` / `cmd start`) detached with stdio nulled so it can't touch the TUI's
/// terminal. Any failure is ignored here; auth flows that need user-visible diagnostics should
/// call [`open_in_browser_checked`] directly.
pub fn open_in_browser(url: &str) {
    let _ = open_in_browser_checked(url);
}

/// Open `url` in the system browser and return enough detail for auth surfaces to explain a
/// definite failure. A helper that stays alive after [`EARLY_EXIT_WINDOW`] is treated as a
/// successful handoff because several desktop openers keep a broker process around.
pub fn open_in_browser_checked(url: &str) -> BrowserOpenReport {
    let mut attempts = Vec::new();
    for candidate in open_candidates(url) {
        let attempt = run_candidate(candidate);
        let launched = matches!(
            attempt.outcome,
            BrowserOpenOutcome::Exited { code: Some(0), .. } | BrowserOpenOutcome::StillRunning
        );
        attempts.push(attempt);
        if launched {
            return BrowserOpenReport {
                status: BrowserOpenStatus::Launched,
                attempts,
            };
        }
    }
    BrowserOpenReport {
        status: BrowserOpenStatus::Failed,
        attempts,
    }
}

/// Open a local file or folder with the system's default handler (the same platform openers
/// as [`open_in_browser`], which all accept a path). Fire-and-forget.
pub fn open_path(path: &std::path::Path) {
    open_in_browser(&path.to_string_lossy());
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenCandidate {
    opener: String,
    program: String,
    args: Vec<String>,
}

fn open_candidates(url: &str) -> Vec<OpenCandidate> {
    if cfg!(target_os = "macos") {
        vec![candidate("open", "open", [url])]
    } else if cfg!(target_os = "windows") {
        vec![candidate("cmd start", "cmd", ["/C", "start", "", url])]
    } else {
        linux_open_candidates(url, std::env::var("BROWSER").ok().as_deref(), detect_wsl())
    }
}

fn linux_open_candidates(url: &str, browser_env: Option<&str>, is_wsl: bool) -> Vec<OpenCandidate> {
    let mut out = Vec::new();
    out.push(candidate("xdg-open", "xdg-open", [url]));
    if is_wsl {
        out.push(candidate("wslview", "wslview", [url]));
        out.push(candidate("wsl-open", "wsl-open", [url]));
        out.push(candidate(
            "powershell.exe Start-Process",
            "powershell.exe",
            ["-NoProfile", "-Command", "Start-Process", url],
        ));
    }
    out.extend([
        candidate("gio open", "gio", ["open", url]),
        candidate("kde-open5", "kde-open5", [url]),
        candidate("kde-open", "kde-open", [url]),
        candidate("gnome-open", "gnome-open", [url]),
        candidate("exo-open", "exo-open", [url]),
    ]);
    if let Some(browser) = browser_env {
        out.extend(browser_env_candidates(url, browser).into_iter().take(3));
    }
    out
}

fn candidate<'a>(
    opener: impl Into<String>,
    program: impl Into<String>,
    args: impl IntoIterator<Item = &'a str>,
) -> OpenCandidate {
    OpenCandidate {
        opener: opener.into(),
        program: program.into(),
        args: args.into_iter().map(str::to_owned).collect(),
    }
}

fn run_candidate(candidate: OpenCandidate) -> BrowserOpenAttempt {
    let mut cmd = process::std_command(&candidate.program, process::ProcessProfile::DesktopOpen);
    cmd.args(&candidate.args);
    let outcome = run_command(&mut cmd);
    BrowserOpenAttempt {
        opener: candidate.opener,
        outcome,
    }
}

fn run_command(cmd: &mut Command) -> BrowserOpenOutcome {
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return BrowserOpenOutcome::SpawnFailed(e.to_string()),
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = child.stderr.take().map(read_tail).unwrap_or_default();
                return BrowserOpenOutcome::Exited {
                    code: status.code(),
                    stderr,
                };
            }
            Ok(None) if start.elapsed() < EARLY_EXIT_WINDOW => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => return BrowserOpenOutcome::StillRunning,
            Err(e) => return BrowserOpenOutcome::SpawnFailed(e.to_string()),
        }
    }
}

fn read_tail(mut reader: impl Read) -> String {
    let mut buf = Vec::new();
    let _ = reader
        .by_ref()
        .take((STDERR_TAIL_MAX + 1) as u64)
        .read_to_end(&mut buf);
    if buf.len() > STDERR_TAIL_MAX {
        let excess = buf.len() - STDERR_TAIL_MAX;
        buf.drain(..excess);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn detect_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn browser_env_candidates(url: &str, browser_env: &str) -> Vec<OpenCandidate> {
    browser_env
        .split(':')
        .filter_map(|entry| parse_browser_env_entry(url, entry.trim()))
        .collect()
}

fn parse_browser_env_entry(url: &str, entry: &str) -> Option<OpenCandidate> {
    if entry.is_empty()
        || entry.chars().any(|c| {
            matches!(
                c,
                ';' | '&' | '|' | '`' | '$' | '>' | '<' | '\n' | '\r' | '"'
            )
        })
    {
        return None;
    }
    let mut parts: Vec<String> = entry.split_whitespace().map(str::to_owned).collect();
    let program = parts.first()?.clone();
    let base = program.rsplit('/').next().unwrap_or(&program);
    if matches!(base, "echo" | "printf" | "true" | "false") {
        return None;
    }
    let mut replaced = false;
    for arg in parts.iter_mut().skip(1) {
        if arg.contains("%s") {
            *arg = arg.replace("%s", url);
            replaced = true;
        }
    }
    if !replaced {
        parts.push(url.to_owned());
    }
    Some(OpenCandidate {
        opener: format!("$BROWSER:{base}"),
        program,
        args: parts.into_iter().skip(1).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_candidates_start_with_xdg_and_include_desktop_fallbacks() {
        let candidates = linux_open_candidates("https://example.test", None, false);
        let openers: Vec<_> = candidates.iter().map(|c| c.opener.as_str()).collect();
        assert_eq!(openers.first(), Some(&"xdg-open"));
        assert!(openers.contains(&"gio open"));
        assert!(openers.contains(&"kde-open5"));
        assert!(!openers.contains(&"wslview"));
    }

    #[test]
    fn linux_candidates_include_wsl_fallbacks_when_detected() {
        let candidates = linux_open_candidates("https://example.test", None, true);
        let openers: Vec<_> = candidates.iter().map(|c| c.opener.as_str()).collect();
        assert!(openers.contains(&"wslview"));
        assert!(openers.contains(&"wsl-open"));
        assert!(openers.contains(&"powershell.exe Start-Process"));
    }

    #[test]
    fn browser_env_candidates_are_shell_free() {
        let candidates = browser_env_candidates(
            "https://example.test",
            "firefox --new-tab %s:echo %s:bad;rm:chromium",
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.opener.as_str())
                .collect::<Vec<_>>(),
            vec!["$BROWSER:firefox", "$BROWSER:chromium"]
        );
        assert_eq!(
            candidates[0].args,
            vec!["--new-tab".to_string(), "https://example.test".to_string()]
        );
        assert_eq!(candidates[1].args, vec!["https://example.test".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn command_failure_captures_exit_and_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo opener failed >&2; exit 7"]);
        match run_command(&mut cmd) {
            BrowserOpenOutcome::Exited { code, stderr } => {
                assert_eq!(code, Some(7));
                assert!(stderr.contains("opener failed"));
            }
            other => panic!("expected failed exit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn long_running_command_is_treated_as_handoff() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 2"]);
        assert_eq!(run_command(&mut cmd), BrowserOpenOutcome::StillRunning);
    }
}
