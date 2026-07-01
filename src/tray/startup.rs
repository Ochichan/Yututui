//! Opt-in desktop startup helpers for the tray companion.

use std::fmt;
use std::path::Path;

#[cfg(windows)]
const RUN_VALUE_NAME: &str = "YtmTui Tray";

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
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::Unsupported => {
                write!(f, "startup management is only supported on Windows")
            }
            StartupError::CurrentExe(message) => write!(f, "{message}"),
            StartupError::Registry(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for StartupError {}

pub fn install() -> Result<String, StartupError> {
    let exe = std::env::current_exe()
        .map_err(|e| StartupError::CurrentExe(format!("could not resolve ytt-tray path: {e}")))?;
    install_for_exe(&exe)
}

pub fn uninstall() -> Result<(), StartupError> {
    platform_uninstall()
}

pub fn status() -> Result<StartupStatus, StartupError> {
    platform_status()
}

fn install_for_exe(exe: &Path) -> Result<String, StartupError> {
    let command = startup_command_for(exe);
    platform_install(&command)?;
    Ok(command)
}

fn startup_command_for(exe: &Path) -> String {
    format!("\"{}\" --background", exe.to_string_lossy())
}

#[cfg(windows)]
fn platform_install(command: &str) -> Result<(), StartupError> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };

    let mut key: HKEY = null_mut();
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
    unsafe {
        RegCloseKey(key);
    }
    if rc != ERROR_SUCCESS {
        return Err(registry_error("write HKCU Run value", rc));
    }
    Ok(())
}

#[cfg(not(windows))]
fn platform_install(_command: &str) -> Result<(), StartupError> {
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

    let rc = unsafe { RegDeleteValueW(key, wide(RUN_VALUE_NAME).as_ptr()) };
    unsafe {
        RegCloseKey(key);
    }
    if rc == ERROR_FILE_NOT_FOUND || rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error("delete HKCU Run value", rc))
    }
}

#[cfg(not(windows))]
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
        unsafe {
            RegCloseKey(key);
        }
        return Ok(StartupStatus::Disabled);
    }
    if rc != ERROR_SUCCESS {
        unsafe {
            RegCloseKey(key);
        }
        return Err(registry_error("query HKCU Run value", rc));
    }
    if value_type != REG_SZ || bytes == 0 {
        unsafe {
            RegCloseKey(key);
        }
        return Ok(StartupStatus::Disabled);
    }

    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
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

#[cfg(not(windows))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn startup_command_quotes_exe_and_uses_background_flag() {
        let command = startup_command_for(Path::new(r"C:\Program Files\YtmTui\ytt-tray.exe"));
        assert_eq!(
            command,
            r#""C:\Program Files\YtmTui\ytt-tray.exe" --background"#
        );
    }
}
