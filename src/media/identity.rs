//! Windows shell identity for the OS media session (the "Unknown app" fix).
//!
//! The Windows media flyout resolves an app's display name and icon from its
//! AppUserModelID; an unpackaged exe with no registered AUMID shows as
//! "Unknown app" with no logo (observed by mpv PR #14338 and souvlaki #67).
//! Two halves, both cosmetic and both safe to skip:
//!
//! - Every `ytt` process stamps its AUMID at startup
//!   ([`adopt_process_identity`]) — TUI and the daemon child alike, since the
//!   SMTC session lives in whichever of them plays (never the tray process).
//! - An opt-in, idempotent registration (`ytt register-media-identity`, run by
//!   `install.ps1` and the Scoop `post_install`) writes
//!   `HKCU\Software\Classes\AppUserModelId\<AUMID>` with `DisplayName` +
//!   `IconUri` — the mpv `--register` precedent. A Start-Menu shortcut
//!   carrying the AUMID property is the documented fallback if real-Windows QA
//!   shows the registry route alone doesn't rename the flyout entry
//!   (docs/windows-smtc-completion-plan.md §4). Registration is never run
//!   implicitly at startup: daemon/SSH/CI invocations must not write registry
//!   keys as a side effect.

/// The `ytt` process AUMID. Deliberately distinct from the tray shell's
/// `io.github.ochi.ytm-tui.tray`: the tray never owns the media session, and
/// sharing an id would only entangle taskbar grouping between the two exes.
pub const APP_USER_MODEL_ID: &str = "io.github.ochi.ytm-tui";

#[cfg(windows)]
const DISPLAY_NAME: &str = "YtmTui";
/// Icon file shipped at the archive root next to the exes (see build.yml
/// packaging); the register command defaults to the copy beside the exe.
#[cfg(windows)]
const ICON_FILE: &str = "ytm-tui.ico";

/// Tag this process with the app's AUMID for shell surfaces (media flyout,
/// taskbar grouping). Must run before any window or media session exists.
/// Failure is harmless and this runs before logging is up — ignored.
#[cfg(windows)]
pub fn adopt_process_identity() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    let id = wide(APP_USER_MODEL_ID);
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}

/// `ytt register-media-identity [--icon <path>]` (hidden; not in `--help`).
#[cfg(windows)]
pub fn register_cli(args: &[String]) -> i32 {
    let mut icon: Option<std::path::PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("Usage: ytt register-media-identity [--icon <path-to-ico>]");
                println!();
                println!("Registers this app's AppUserModelId (HKCU only) so Windows media");
                println!(
                    "surfaces show \"{DISPLAY_NAME}\" and its icon instead of \"Unknown app\"."
                );
                println!("Idempotent; run automatically by install.ps1 and Scoop.");
                println!("Default icon: {ICON_FILE} next to ytt.exe, when present.");
                return 0;
            }
            "--icon" => match iter.next() {
                Some(path) => icon = Some(std::path::PathBuf::from(path)),
                None => {
                    eprintln!("--icon requires a path");
                    return 2;
                }
            },
            other => {
                eprintln!("unknown argument: {other}");
                return 2;
            }
        }
    }

    let icon = icon.or_else(exe_adjacent_icon);
    let icon = match icon {
        Some(path) if !path.is_file() => {
            eprintln!("icon not found: {}", path.display());
            return 2;
        }
        // The registry wants an absolute path; `ytt register-media-identity
        // --icon ytm-tui.ico` from the archive dir should still stick after cd.
        Some(path) => match std::path::absolute(&path) {
            Ok(absolute) => Some(absolute),
            Err(_) => Some(path),
        },
        None => None,
    };

    match write_registration(icon.as_deref()) {
        Ok(()) => {
            println!("Registered media identity: {DISPLAY_NAME} ({APP_USER_MODEL_ID})");
            match icon {
                Some(path) => println!("  icon: {}", path.display()),
                None => println!("  icon: none ({ICON_FILE} not found next to ytt.exe)"),
            }
            0
        }
        Err(message) => {
            eprintln!("media identity registration failed: {message}");
            1
        }
    }
}

#[cfg(not(windows))]
pub fn register_cli(_args: &[String]) -> i32 {
    eprintln!("register-media-identity is a Windows-only maintenance command.");
    2
}

#[cfg(windows)]
fn exe_adjacent_icon() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(ICON_FILE);
    path.is_file().then_some(path)
}

#[cfg(windows)]
fn write_registration(icon: Option<&std::path::Path>) -> Result<(), String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegSetValueExW,
    };

    let key_path = format!(r"Software\Classes\AppUserModelId\{APP_USER_MODEL_ID}");
    let mut key: HKEY = null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(&key_path).as_ptr(),
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
        return Err(format!("could not open HKCU\\{key_path} (rc {rc})"));
    }

    let set = |name: &str, value: &str| -> Result<(), String> {
        let data = wide(value);
        let bytes =
            u32::try_from(data.len() * 2).map_err(|_| format!("value for {name} is too long"))?;
        let rc = unsafe {
            RegSetValueExW(
                key,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                bytes,
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("could not write {name} (rc {rc})"))
        }
    };

    let mut result = set("DisplayName", DISPLAY_NAME);
    if result.is_ok()
        && let Some(icon) = icon
    {
        result = set("IconUri", &icon.display().to_string());
    }
    unsafe {
        RegCloseKey(key);
    }
    result
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
