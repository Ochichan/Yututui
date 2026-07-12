//! Opt-in desktop startup helpers for the tray companion.

use std::fmt;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(windows)]
const RUN_VALUE_NAME: &str = "YuTuTray!";
#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "io.github.ochi.yututui.tray";
#[cfg(target_os = "macos")]
const LAUNCH_AGENT_FILE: &str = "io.github.ochi.yututui.tray.plist";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupStatus {
    Enabled { command: String },
    Disabled,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupError {
    Unsupported,
    CurrentExe(String),
    Registry(String),
    LaunchAgent(String),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::Unsupported => {
                write!(
                    f,
                    "startup management is only supported on Windows and macOS"
                )
            }
            StartupError::CurrentExe(message) => write!(f, "{message}"),
            StartupError::Registry(message) => write!(f, "{message}"),
            StartupError::LaunchAgent(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for StartupError {}

pub fn install() -> Result<String, StartupError> {
    let exe = std::env::current_exe()
        .map_err(|e| StartupError::CurrentExe(format!("could not resolve yututray path: {e}")))?;
    install_for_exe(&exe)
}

/// Re-register the login-startup entry when it points at a stale exe path — a rename
/// (`yututray` → `yututray`) or a moved install (docs/gui/03 §1.3). No-op unless startup
/// is currently enabled. The registry value-name / LaunchAgent label are stable (the
/// `io.github.ochi.yututui.tray` family), so re-installing overwrites rather than duplicates.
/// Best-effort: called on every desktop start so upgrades heal themselves silently.
pub fn self_heal() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let want = startup_command_for(&exe);
    if let Ok(StartupStatus::Enabled { command }) = status()
        && command != want
    {
        let _ = install();
    }
}

pub fn uninstall() -> Result<(), StartupError> {
    platform_uninstall()
}

pub fn status() -> Result<StartupStatus, StartupError> {
    platform_status()
}

fn install_for_exe(exe: &Path) -> Result<String, StartupError> {
    let command = startup_command_for(exe);
    platform_install(exe, &command)?;
    Ok(command)
}

fn startup_command_for(exe: &Path) -> String {
    format!("\"{}\" --background", exe.to_string_lossy())
}

#[cfg(windows)]
fn platform_install(_exe: &Path, command: &str) -> Result<(), StartupError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };

    let mut key: HKEY = null_mut();
    // SAFETY: `RUN_KEY` is passed as a NUL-terminated UTF-16 string; output `key`
    // points to valid storage and the return code is checked before use.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null_mut(),
            &mut key,
            null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(registry_error("open HKCU Run", rc));
    }

    let command_wide = wide(command);
    let bytes = bytes_len(&command_wide)?;
    // SAFETY: `key` is a successfully opened registry handle; `command_wide` is
    // NUL-terminated UTF-16 and `bytes` covers the buffer in bytes.
    let rc = unsafe {
        RegSetValueExW(
            key,
            wide(RUN_VALUE_NAME).as_ptr(),
            0,
            REG_SZ,
            command_wide.as_ptr().cast(),
            bytes,
        )
    };
    // SAFETY: `key` is owned by this function after RegCreateKeyExW succeeds and is
    // closed exactly once before returning.
    unsafe {
        RegCloseKey(key);
    }
    if rc != ERROR_SUCCESS {
        return Err(registry_error("write HKCU Run value", rc));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_install(exe: &Path, _command: &str) -> Result<(), StartupError> {
    let paths = macos_startup_paths()?;
    std::fs::create_dir_all(&paths.launch_agents_dir).map_err(|e| {
        StartupError::LaunchAgent(format!(
            "could not create LaunchAgents directory {}: {e}",
            paths.launch_agents_dir.display()
        ))
    })?;
    std::fs::create_dir_all(&paths.log_dir).map_err(|e| {
        StartupError::LaunchAgent(format!(
            "could not create startup log directory {}: {e}",
            paths.log_dir.display()
        ))
    })?;

    let plist = launch_agent_plist(exe, &paths.stdout_log, &paths.stderr_log);
    // Write-then-rename so a crash mid-write can't leave a truncated agent that
    // launchd rejects while status() still reports it as Enabled.
    let tmp = paths.plist.with_extension("plist.tmp");
    std::fs::write(&tmp, plist).map_err(|e| {
        StartupError::LaunchAgent(format!(
            "could not write LaunchAgent {}: {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, &paths.plist).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StartupError::LaunchAgent(format!(
            "could not install LaunchAgent {}: {e}",
            paths.plist.display()
        ))
    })
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_install(_exe: &Path, _command: &str) -> Result<(), StartupError> {
    Err(StartupError::Unsupported)
}

#[cfg(windows)]
fn platform_uninstall() -> Result<(), StartupError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    };

    let mut key: HKEY = null_mut();
    // SAFETY: `RUN_KEY` is a NUL-terminated UTF-16 string; output `key` storage is
    // valid and the return code is checked before any handle use.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if rc != ERROR_SUCCESS {
        return Err(registry_error("open HKCU Run", rc));
    }

    // SAFETY: `key` is a valid open registry handle and the value name is
    // NUL-terminated for the duration of the call.
    let rc = unsafe { RegDeleteValueW(key, wide(RUN_VALUE_NAME).as_ptr()) };
    // SAFETY: `key` is owned by this function after RegOpenKeyExW succeeds and is
    // closed exactly once before returning.
    unsafe {
        RegCloseKey(key);
    }
    if rc == ERROR_FILE_NOT_FOUND || rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error("delete HKCU Run value", rc))
    }
}

#[cfg(target_os = "macos")]
fn platform_uninstall() -> Result<(), StartupError> {
    let paths = macos_startup_paths()?;
    match std::fs::remove_file(&paths.plist) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StartupError::LaunchAgent(format!(
            "could not remove LaunchAgent {}: {e}",
            paths.plist.display()
        ))),
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_uninstall() -> Result<(), StartupError> {
    Err(StartupError::Unsupported)
}

#[cfg(windows)]
fn platform_status() -> Result<StartupStatus, StartupError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };

    let mut key: HKEY = null_mut();
    // SAFETY: `RUN_KEY` is a NUL-terminated UTF-16 string; output `key` storage is
    // valid and the return code is checked before any handle use.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(StartupStatus::Disabled);
    }
    if rc != ERROR_SUCCESS {
        return Err(registry_error("open HKCU Run", rc));
    }

    let name = wide(RUN_VALUE_NAME);
    let mut value_type = 0u32;
    let mut bytes = 0u32;
    // SAFETY: `key` and `name` are valid; null data buffer requests the required byte
    // size, which Windows writes into `bytes`.
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut bytes,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        // SAFETY: close the valid query handle before returning Disabled.
        unsafe {
            RegCloseKey(key);
        }
        return Ok(StartupStatus::Disabled);
    }
    if rc != ERROR_SUCCESS {
        // SAFETY: close the valid query handle before returning the registry error.
        unsafe {
            RegCloseKey(key);
        }
        return Err(registry_error("query HKCU Run value", rc));
    }
    if value_type != REG_SZ || bytes == 0 {
        // SAFETY: close the valid query handle before returning Disabled.
        unsafe {
            RegCloseKey(key);
        }
        return Ok(StartupStatus::Disabled);
    }

    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: `buffer` has at least the byte length reported by the size query;
    // Windows writes UTF-16 data and updates `bytes`.
    let rc = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null_mut(),
            &mut value_type,
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    // SAFETY: `key` is owned by this function after RegOpenKeyExW succeeds and is
    // closed exactly once after the final query.
    unsafe {
        RegCloseKey(key);
    }
    if rc != ERROR_SUCCESS {
        return Err(registry_error("read HKCU Run value", rc));
    }
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    Ok(StartupStatus::Enabled {
        command: String::from_utf16_lossy(&buffer),
    })
}

#[cfg(target_os = "macos")]
fn platform_status() -> Result<StartupStatus, StartupError> {
    let paths = macos_startup_paths()?;
    if !paths.plist.exists() {
        return Ok(StartupStatus::Disabled);
    }
    let contents = std::fs::read_to_string(&paths.plist).map_err(|e| {
        StartupError::LaunchAgent(format!(
            "could not read LaunchAgent {}: {e}",
            paths.plist.display()
        ))
    })?;
    let command = launch_agent_program_arguments(&contents)
        .map(|args| shell_command(&args))
        .unwrap_or_else(|| paths.plist.display().to_string());
    Ok(StartupStatus::Enabled { command })
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn platform_status() -> Result<StartupStatus, StartupError> {
    Ok(StartupStatus::Unsupported)
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn bytes_len(value: &[u16]) -> Result<u32, StartupError> {
    value
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| StartupError::Registry("registry value is too large".to_string()))
}

#[cfg(windows)]
fn registry_error(action: &str, code: u32) -> StartupError {
    StartupError::Registry(format!("{action} failed with Windows error {code}"))
}

#[cfg(target_os = "macos")]
struct MacStartupPaths {
    launch_agents_dir: PathBuf,
    plist: PathBuf,
    log_dir: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

#[cfg(target_os = "macos")]
fn macos_startup_paths() -> Result<MacStartupPaths, StartupError> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| StartupError::LaunchAgent("could not resolve home directory".to_string()))?;
    let launch_agents_dir = base.home_dir().join("Library/LaunchAgents");
    let log_dir = crate::desktop::persistence::cache_dir()
        .map(|dir| dir.join("logs"))
        .unwrap_or_else(|| base.home_dir().join("Library/Caches/yututray/logs"));
    Ok(MacStartupPaths {
        plist: launch_agents_dir.join(LAUNCH_AGENT_FILE),
        launch_agents_dir,
        stdout_log: log_dir.join("tray-launchagent.out.log"),
        stderr_log: log_dir.join("tray-launchagent.err.log"),
        log_dir,
    })
}

#[cfg(target_os = "macos")]
fn launch_agent_plist(exe: &Path, stdout_log: &Path, stderr_log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--background</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        plist_escape(LAUNCH_AGENT_LABEL),
        plist_escape(&exe.to_string_lossy()),
        plist_escape(&stdout_log.to_string_lossy()),
        plist_escape(&stderr_log.to_string_lossy())
    )
}

#[cfg(target_os = "macos")]
fn launch_agent_program_arguments(plist: &str) -> Option<Vec<String>> {
    let start = plist.find("<key>ProgramArguments</key>")?;
    let after_key = &plist[start..];
    let array_start = after_key.find("<array>")? + "<array>".len();
    let after_array = &after_key[array_start..];
    let array_end = after_array.find("</array>")?;
    let mut rest = &after_array[..array_end];
    let mut args = Vec::new();

    while let Some(open) = rest.find("<string>") {
        rest = &rest[open + "<string>".len()..];
        let close = rest.find("</string>")?;
        args.push(plist_unescape(&rest[..close]));
        rest = &rest[close + "</string>".len()..];
    }

    (!args.is_empty()).then_some(args)
}

#[cfg(target_os = "macos")]
fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn plist_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn plist_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn startup_command_quotes_exe_and_uses_background_flag() {
        let command = startup_command_for(Path::new(r"C:\Program Files\YuTuTui!\yututray.exe"));
        assert_eq!(
            command,
            r#""C:\Program Files\YuTuTui!\yututray.exe" --background"#
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_agent_plist_round_trips_program_arguments() {
        let plist = launch_agent_plist(
            Path::new("/Applications/YuTuTray!.app/Contents/MacOS/yututray"),
            Path::new("/Users/me/Library/Caches/yututui/logs/out.log"),
            Path::new("/Users/me/Library/Caches/yututui/logs/err.log"),
        );
        let args = launch_agent_program_arguments(&plist).unwrap();
        assert_eq!(
            args,
            vec![
                "/Applications/YuTuTray!.app/Contents/MacOS/yututray".to_string(),
                "--background".to_string()
            ]
        );
        assert!(shell_command(&args).contains("--background"));
    }
}
