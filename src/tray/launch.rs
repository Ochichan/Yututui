//! Terminal-launch planning for the desktop companion.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::util::process::{self, ProcessProfile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    kind: LaunchPlanKind,
}

impl LaunchPlan {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            kind: LaunchPlanKind::Direct,
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_default_terminal(ytt_path: &Path) -> Self {
        Self {
            program: "open".to_string(),
            args: vec!["<LaunchServices default .command terminal>".to_string()],
            kind: LaunchPlanKind::MacosCommandScript {
                ytt_path: ytt_path.to_path_buf(),
                app: None,
            },
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_terminal_app(ytt_path: &Path) -> Self {
        Self {
            program: "osascript".to_string(),
            args: vec![
                "-e".to_string(),
                format!(
                    "tell application \"Terminal\" to do script {}",
                    applescript_string(&shell_quote(ytt_path))
                ),
            ],
            kind: LaunchPlanKind::Direct,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchPlanKind {
    Direct,
    #[cfg(target_os = "macos")]
    MacosCommandScript {
        ytt_path: PathBuf,
        app: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchError {
    pub attempts: Vec<(LaunchPlan, String)>,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not launch ytt in a terminal")?;
        if let Some((plan, err)) = self.attempts.last() {
            write!(f, " (last attempt: {}: {err})", plan.program)?;
        }
        Ok(())
    }
}

impl std::error::Error for LaunchError {}

pub fn resolve_ytt_path() -> PathBuf {
    let exe = std::env::current_exe().ok();
    let sibling_name = if cfg!(windows) { "ytt.exe" } else { "ytt" };
    if let Some(path) = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|dir| dir.join(sibling_name))
        .filter(|p| p.exists())
    {
        return path;
    }
    PathBuf::from(sibling_name)
}

pub fn open_tui() -> Result<LaunchPlan, LaunchError> {
    open_tui_with_path(&resolve_ytt_path())
}

pub fn open_tui_with_path(ytt_path: &Path) -> Result<LaunchPlan, LaunchError> {
    let terminal = std::env::var("TERMINAL").ok();
    let plans = candidate_plans_for(ytt_path, terminal.as_deref());
    let mut attempts = Vec::new();
    for plan in plans {
        let (plan, cleanup) = match materialize_plan(plan) {
            Ok(plan) => plan,
            Err((plan, error)) => {
                attempts.push((plan, error));
                continue;
            }
        };
        let mut cmd = process::std_command(&plan.program, ProcessProfile::DesktopOpen);
        cmd.args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(_) => return Ok(plan),
            Err(e) => {
                if let Some(path) = cleanup {
                    let _ = std::fs::remove_file(path);
                }
                attempts.push((plan, e.to_string()));
            }
        }
    }
    Err(LaunchError { attempts })
}

pub fn candidate_plans_for(ytt_path: &Path, terminal_env: Option<&str>) -> Vec<LaunchPlan> {
    platform_candidate_plans(ytt_path, terminal_env)
}

#[cfg(target_os = "macos")]
fn platform_candidate_plans(ytt_path: &Path, _terminal_env: Option<&str>) -> Vec<LaunchPlan> {
    vec![
        LaunchPlan::macos_default_terminal(ytt_path),
        LaunchPlan::macos_terminal_app(ytt_path),
    ]
}

#[cfg(target_os = "macos")]
fn materialize_plan(
    plan: LaunchPlan,
) -> Result<(LaunchPlan, Option<PathBuf>), (LaunchPlan, String)> {
    match plan.kind.clone() {
        LaunchPlanKind::Direct => Ok((plan, None)),
        LaunchPlanKind::MacosCommandScript { ytt_path, app } => {
            let script = match write_macos_command_script(&ytt_path) {
                Ok(path) => path,
                Err(e) => return Err((plan, e.to_string())),
            };
            let mut args = Vec::new();
            if let Some(app) = app {
                args.extend(["-a".to_string(), app]);
            }
            args.push(script.to_string_lossy().into_owned());
            Ok((LaunchPlan::new("open", args), Some(script)))
        }
    }
}

#[cfg(target_os = "windows")]
fn platform_candidate_plans(ytt_path: &Path, _terminal_env: Option<&str>) -> Vec<LaunchPlan> {
    windows_candidate_plans(ytt_path)
}

#[cfg(any(target_os = "windows", test))]
fn windows_candidate_plans(ytt_path: &Path) -> Vec<LaunchPlan> {
    let path = ytt_path.to_string_lossy().into_owned();
    vec![
        LaunchPlan::new("wt.exe", vec![path.clone()]),
        LaunchPlan::new(
            "powershell.exe",
            vec![
                "-NoExit".to_string(),
                "-Command".to_string(),
                format!("& {}", powershell_quote(&path)),
            ],
        ),
        LaunchPlan::new("cmd.exe", vec!["/K".to_string(), cmd_quote(&path)]),
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_candidate_plans(ytt_path: &Path, terminal_env: Option<&str>) -> Vec<LaunchPlan> {
    let path = ytt_path.to_string_lossy().into_owned();
    let mut plans = Vec::new();
    if let Some(term) = terminal_env.filter(|s| !s.trim().is_empty()) {
        plans.push(LaunchPlan::new(term, vec!["-e".to_string(), path.clone()]));
    }
    plans.extend([
        LaunchPlan::new("x-terminal-emulator", vec!["-e".to_string(), path.clone()]),
        LaunchPlan::new("gnome-terminal", vec!["--".to_string(), path.clone()]),
        LaunchPlan::new("konsole", vec!["-e".to_string(), path.clone()]),
        LaunchPlan::new("xfce4-terminal", vec!["-e".to_string(), path.clone()]),
        LaunchPlan::new("alacritty", vec!["-e".to_string(), path.clone()]),
        LaunchPlan::new("kitty", vec![path.clone()]),
        LaunchPlan::new("wezterm", vec!["start".to_string(), "--".to_string(), path]),
    ]);
    plans
}

#[cfg(not(target_os = "macos"))]
fn materialize_plan(
    plan: LaunchPlan,
) -> Result<(LaunchPlan, Option<PathBuf>), (LaunchPlan, String)> {
    Ok((plan, None))
}

#[cfg(target_os = "macos")]
fn applescript_string(input: &str) -> String {
    let escaped = input.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "macos")]
fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn write_macos_command_script(ytt_path: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "ytm-tui-open-tui-{}-{nonce}.command",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(
        format!(
            "#!/bin/sh\ncd \"$HOME\" || exit 1\nrm -f -- \"$0\"\nexec {}\n",
            shell_quote(ytt_path)
        )
        .as_bytes(),
    )?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions)?;
    Ok(path)
}

#[cfg(any(target_os = "windows", test))]
fn powershell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "''"))
}

#[cfg(any(target_os = "windows", test))]
fn cmd_quote(input: &str) -> String {
    format!("\"{}\"", input.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_plan_uses_launchservices_default_terminal_first() {
        let plans = candidate_plans_for(Path::new("/Applications/Ytm Tui/ytt"), None);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].program, "open");
        assert!(
            plans[0].args[0].contains("LaunchServices"),
            "dry-run should explain that `open` delegates to LaunchServices"
        );
        assert_eq!(plans[1].program, "osascript");
        assert!(plans[1].args[1].contains("Terminal"));
        assert!(plans[1].args[1].contains("/Applications/Ytm Tui/ytt"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_command_script_materializes_to_open_plan() {
        let plan = LaunchPlan::macos_default_terminal(Path::new("/Applications/Ytm Tui/ytt"));
        let (plan, cleanup) = materialize_plan(plan).unwrap();
        assert_eq!(plan.program, "open");
        let script = cleanup.expect("script should be created for LaunchServices");
        assert_eq!(plan.args, vec![script.to_string_lossy().into_owned()]);
        let script_text = std::fs::read_to_string(&script).unwrap();
        assert!(script_text.contains("exec '/Applications/Ytm Tui/ytt'"));
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn windows_plans_prefer_windows_terminal() {
        let plans = windows_candidate_plans(Path::new(r"C:\Program Files\YtmTui\ytt.exe"));
        assert_eq!(plans[0].program, "wt.exe");
        assert_eq!(plans[1].program, "powershell.exe");
        assert_eq!(plans[2].program, "cmd.exe");
        assert!(plans[1].args[2].contains("C:\\Program Files\\YtmTui\\ytt.exe"));
    }

    #[test]
    fn windows_fallbacks_quote_absolute_ytt_path() {
        let plans = windows_candidate_plans(Path::new(r"C:\Users\Ochi Music\ytt.exe"));
        assert_eq!(plans[0].args, vec![r"C:\Users\Ochi Music\ytt.exe"]);
        assert_eq!(
            plans[1].args,
            vec!["-NoExit", "-Command", r"& 'C:\Users\Ochi Music\ytt.exe'"]
        );
        assert_eq!(
            plans[2].args,
            vec!["/K", r#""C:\Users\Ochi Music\ytt.exe""#]
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_plans_honor_terminal_env_first() {
        let plans = candidate_plans_for(Path::new("/usr/bin/ytt"), Some("alacritty"));
        assert_eq!(plans[0].program, "alacritty");
        assert_eq!(plans[0].args, vec!["-e", "/usr/bin/ytt"]);
        assert!(
            plans
                .iter()
                .any(|plan| plan.program == "x-terminal-emulator")
        );
    }
}
