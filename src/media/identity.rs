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
/// `io.github.ochi.yututui.tray`: the tray never owns the media session, and
/// sharing an id would only entangle taskbar grouping between the two exes.
pub const APP_USER_MODEL_ID: &str = "io.github.ochi.yututui";

#[cfg(windows)]
const DISPLAY_NAME: &str = "YuTuTui!";
/// Icon file shipped at the archive root next to the exes (see build.yml
/// packaging); the register command defaults to the copy beside the exe.
#[cfg(windows)]
const ICON_FILE: &str = "yututui.ico";

/// Tag this process with the app's AUMID for shell surfaces (media flyout,
/// taskbar grouping). Must run before any window or media session exists.
/// Failure is harmless and this runs before logging is up — ignored.
#[cfg(windows)]
pub fn adopt_process_identity() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    let id = wide(APP_USER_MODEL_ID);
    // SAFETY: `id` is NUL-terminated UTF-16 and lives for the Shell call; failure is
    // intentionally ignored because process identity is cosmetic.
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
        // --icon yututui.ico` from the archive dir should still stick after cd.
        Some(path) => match std::path::absolute(&path) {
            Ok(absolute) => Some(absolute),
            Err(_) => Some(path),
        },
        None => None,
    };

    if let Err(message) = write_registration(icon.as_deref()) {
        eprintln!("media identity registration failed: {message}");
        return 1;
    }
    println!("Registered media identity: {DISPLAY_NAME} ({APP_USER_MODEL_ID})");
    match &icon {
        Some(path) => println!("  icon: {}", path.display()),
        None => println!("  icon: none ({ICON_FILE} not found next to ytt.exe)"),
    }

    // The Start-Menu shortcut is what actually renames the media flyout entry —
    // on-hardware QA proved the HKCU key alone still shows "Unknown app" (plan §4,
    // results doc 2026-07-04). Keep both: the key covers toast identity.
    match write_start_menu_shortcut(icon.as_deref()) {
        Ok(path) => {
            println!("  shortcut: {} (AppUserModelID stamped)", path.display());
            0
        }
        Err(message) => {
            eprintln!("start-menu shortcut failed (flyout may show \"Unknown app\"): {message}");
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
    // SAFETY: `key_path` is provided as NUL-terminated UTF-16; `key` points to valid
    // output storage and the return code gates subsequent handle use.
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
        // SAFETY: `key` is the open registration key; `name` and `data` are
        // NUL-terminated UTF-16 buffers valid for this call.
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
    // SAFETY: `key` is owned by this function after RegCreateKeyExW succeeds and is
    // closed exactly once after all value writes.
    unsafe {
        RegCloseKey(key);
    }
    result
}

/// Write `%APPDATA%\Microsoft\Windows\Start Menu\Programs\YuTuTui!.lnk` targeting this
/// exe, with `System.AppUserModel.ID` stamped to [`APP_USER_MODEL_ID`]. The Windows
/// AppResolver maps a media session's explicit AUMID to a display name/icon **only**
/// through a Start-Menu shortcut carrying that property (the Chrome/Spotify route) —
/// the HKCU `AppUserModelId` key alone leaves the flyout on "Unknown app" (proven
/// on-hardware, Win11 26200). Idempotent: overwrites and re-stamps every run. The
/// readback before returning guards against a silently-ignored property write.
#[cfg(windows)]
fn write_start_menu_shortcut(icon: Option<&std::path::Path>) -> Result<std::path::PathBuf, String> {
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{GUID, Interface, PCWSTR};

    // propkey.h System.AppUserModel.ID — {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5.
    const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
        pid: 5,
    };

    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA is not set")?;
    let programs = std::path::PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs");
    // Must live in the Start Menu: the AppResolver only indexes Start-Menu shortcuts.
    let lnk = programs.join(format!("{DISPLAY_NAME}.lnk"));
    let exe = std::env::current_exe().map_err(|e| format!("could not resolve ytt.exe: {e}"))?;

    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            // SAFETY: this guard is constructed only after successful CoInitializeEx
            // on the current thread, so it balances that COM apartment initialization.
            unsafe { CoUninitialize() };
        }
    }
    // SAFETY: initializes COM for this thread before creating ShellLink COM objects;
    // failure is converted into an error and no COM APIs run before success.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|e| format!("CoInitializeEx failed: {e}"))?;
    let _com = ComGuard;

    let result: windows::core::Result<()> = (|| {
        // SAFETY: COM is initialized on this thread and ShellLink is an in-proc COM
        // class; errors are propagated through the windows Result.
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?;
        // SAFETY: the executable path buffer is NUL-terminated UTF-16 and lives until
        // SetPath returns; ShellLink copies the value.
        unsafe { link.SetPath(PCWSTR(wide(&exe.display().to_string()).as_ptr())) }?;
        if let Some(icon) = icon {
            // SAFETY: the icon path buffer is NUL-terminated UTF-16 and lives until
            // SetIconLocation returns; ShellLink copies the value.
            unsafe { link.SetIconLocation(PCWSTR(wide(&icon.display().to_string()).as_ptr()), 0) }?;
        }

        let store: IPropertyStore = link.cast()?;
        let value = PROPVARIANT::from(APP_USER_MODEL_ID);
        // SAFETY: PKEY_AppUserModel_ID expects a string PROPVARIANT; `value` is kept
        // alive until SetValue returns and the HRESULT is checked.
        unsafe { store.SetValue(&PKEY_APP_USER_MODEL_ID, &value) }?;
        // SAFETY: commits the property store for the live ShellLink COM object; errors
        // are propagated to the caller.
        unsafe { store.Commit() }?;

        let file: IPersistFile = link.cast()?;
        // SAFETY: the shortcut path is NUL-terminated UTF-16 and lives until Save
        // returns; `true` permits overwrite and HRESULT is checked.
        unsafe { file.Save(PCWSTR(wide(&lnk.display().to_string()).as_ptr()), true) }?;

        // Readback: the stamp must round-trip or the flyout will still say Unknown app.
        // SAFETY: reads the same string property from the live property store; the
        // returned PROPVARIANT owns its data through the windows crate wrapper.
        let back = unsafe { store.GetValue(&PKEY_APP_USER_MODEL_ID) }?;
        let back = back.to_string();
        if back != APP_USER_MODEL_ID {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(-1),
                format!("AUMID readback mismatch: {back:?}"),
            ));
        }
        Ok(())
    })();

    result.map_err(|e| format!("{e}"))?;
    Ok(lnk)
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
