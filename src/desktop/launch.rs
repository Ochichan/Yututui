//! Terminal-launch planning for the desktop companion.

use std::ffi::OsStr;
use std::fmt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;

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
    resolve_ytt_path_from(
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
        // Windows sets USERPROFILE, not HOME; accept either so the per-user
        // candidate dirs below stay reachable everywhere.
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .as_deref(),
    )
    .unwrap_or_else(|| PathBuf::from(ytt_binary_name()))
}

fn resolve_ytt_path_from(
    current_exe: Option<&Path>,
    path_env: Option<&OsStr>,
    home_env: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(path) = current_exe
        .and_then(|p| p.parent())
        .map(|dir| dir.join(ytt_binary_name()))
        .filter(|p| is_executable_file(p))
    {
        return Some(path);
    }

    path_ytt_candidates(path_env)
        .chain(platform_ytt_candidates(home_env))
        .find(|candidate| is_executable_file(candidate))
}

fn ytt_binary_name() -> &'static str {
    if cfg!(windows) { "ytt.exe" } else { "ytt" }
}

fn path_ytt_candidates(path_env: Option<&OsStr>) -> impl Iterator<Item = PathBuf> + '_ {
    path_env
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(ytt_binary_name()))
}

#[cfg(target_os = "macos")]
fn platform_ytt_candidates(home_env: Option<&OsStr>) -> impl Iterator<Item = PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin").join(ytt_binary_name()),
        PathBuf::from("/usr/local/bin").join(ytt_binary_name()),
    ];
    if let Some(home) = home_env {
        let home = PathBuf::from(home);
        candidates.push(home.join(".cargo/bin").join(ytt_binary_name()));
        candidates.push(home.join(".local/bin").join(ytt_binary_name()));
    }
    candidates.into_iter()
}

#[cfg(windows)]
fn platform_ytt_candidates(home_env: Option<&OsStr>) -> impl Iterator<Item = PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = home_env {
        let home = PathBuf::from(home);
        candidates.push(home.join(".cargo").join("bin").join(ytt_binary_name()));
        // Scoop's default shim directory — a startup-launched tray can see a PATH
        // that predates the shell profile, so probe it directly too.
        candidates.push(home.join("scoop").join("shims").join(ytt_binary_name()));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join(ytt_binary_name()),
        );
    }
    candidates.into_iter()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_ytt_candidates(home_env: Option<&OsStr>) -> impl Iterator<Item = PathBuf> {
    let mut candidates = vec![PathBuf::from("/usr/local/bin").join(ytt_binary_name())];
    if let Some(home) = home_env {
        let home = PathBuf::from(home);
        candidates.push(home.join(".cargo/bin").join(ytt_binary_name()));
        candidates.push(home.join(".local/bin").join(ytt_binary_name()));
    }
    candidates.into_iter()
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
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
            Ok(mut child) => {
                // Reap the launcher when it exits — a dropped Child sits as a zombie
                // for the whole life of the long-running tray process on Unix.
                let _ = thread::Builder::new()
                    .name("yututray-reap".to_string())
                    .spawn(move || {
                        let _ = child.wait();
                    });
                return Ok(plan);
            }
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
        // The bare path, not a pre-quoted one: std::process already quotes an arg
        // containing spaces once, so pre-quoting handed cmd.exe `"\"…\""` — a command
        // it can never resolve. With a single level of quotes, cmd's two-quote rule
        // runs the path as-is.
        LaunchPlan::new("cmd.exe", vec!["/K".to_string(), path]),
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
        "yututui-open-tui-{}-{nonce}.command",
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn temp_test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yututui-launch-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_ytt_path_prefers_executable_sibling() {
        let dir = temp_test_dir("sibling");
        let tray = dir.join("yututray");
        let ytt = dir.join("ytt");
        write_executable(&tray);
        write_executable(&ytt);

        assert_eq!(resolve_ytt_path_from(Some(&tray), None, None), Some(ytt));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_ytt_path_uses_path_when_sibling_is_missing() {
        let app_dir = temp_test_dir("missing-sibling");
        let bin_dir = temp_test_dir("path");
        let tray = app_dir.join("yututray");
        let ytt = bin_dir.join("ytt");
        write_executable(&tray);
        write_executable(&ytt);

        assert_eq!(
            resolve_ytt_path_from(Some(&tray), Some(bin_dir.as_os_str()), None),
            Some(ytt)
        );
        let _ = std::fs::remove_dir_all(app_dir);
        let _ = std::fs::remove_dir_all(bin_dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_platform_candidates_include_home_bins() {
        let home = temp_test_dir("home");
        let candidates: Vec<PathBuf> = platform_ytt_candidates(Some(home.as_os_str())).collect();
        assert!(candidates.contains(&home.join(".cargo/bin/ytt")));
        assert!(candidates.contains(&home.join(".local/bin/ytt")));
        let _ = std::fs::remove_dir_all(home);
    }

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
        let plans = windows_candidate_plans(Path::new(r"C:\Program Files\YuTuTui!\ytt.exe"));
        assert_eq!(plans[0].program, "wt.exe");
        assert_eq!(plans[1].program, "powershell.exe");
        assert_eq!(plans[2].program, "cmd.exe");
        assert!(plans[1].args[2].contains("C:\\Program Files\\YuTuTui!\\ytt.exe"));
    }

    #[test]
    fn windows_fallbacks_quote_absolute_ytt_path() {
        let plans = windows_candidate_plans(Path::new(r"C:\Users\Ochi Music\ytt.exe"));
        assert_eq!(plans[0].args, vec![r"C:\Users\Ochi Music\ytt.exe"]);
        assert_eq!(
            plans[1].args,
            vec!["-NoExit", "-Command", r"& 'C:\Users\Ochi Music\ytt.exe'"]
        );
        assert_eq!(plans[2].args, vec!["/K", r"C:\Users\Ochi Music\ytt.exe"]);
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
