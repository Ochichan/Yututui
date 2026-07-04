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
//!   `install.ps1` and the Scoop `post_install`) writes TWO things: (1) the
//!   `HKCU\Software\Classes\AppUserModelId\<AUMID>` key with `DisplayName` +
//!   `IconUri` (the mpv `--register` precedent, kept for toast identity), and
//!   (2) a Start-Menu shortcut (`…\Programs\YtmTui.lnk`) whose
//!   `System.AppUserModel.ID` property equals our AUMID — the route the media
//!   flyout / taskbar actually resolve the name+icon from (as Chrome/Spotify do).
//!
//! On-hardware Win11 QA (docs/windows-smtc-qa-results-2026-07-04.md) proved the
//! registry key ALONE still shows "Unknown app" even after a sign-out/in, and
//! that the AUMID-stamped shortcut flips the flyout to "YtmTui" + icon
//! immediately — so we write both, not one-or-the-other. Registration is never
//! run implicitly at startup: daemon/SSH/CI invocations must not write registry
//! keys or shortcuts as a side effect.

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
                println!("Registers this app's AppUserModelId (HKCU key + a Start-Menu");
                println!(
                    "shortcut) so Windows media surfaces show \"{DISPLAY_NAME}\" and its icon"
                );
                println!("instead of \"Unknown app\".");
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
            match icon.as_deref() {
                Some(path) => println!("  icon: {}", path.display()),
                None => println!("  icon: none ({ICON_FILE} not found next to ytt.exe)"),
            }
            // The Start-Menu shortcut is what the media flyout actually resolves
            // the name+icon from — the HKCU key alone shows "Unknown app" (proven
            // by on-hardware QA). A failure here is a soft warning: the registry
            // write already succeeded, and a shell/COM hiccup must not fail a
            // Scoop `post_install`.
            match write_start_menu_shortcut(icon.as_deref()) {
                Ok(path) => println!("  shortcut: {}", path.display()),
                Err(message) => {
                    eprintln!("  warning: Start-Menu shortcut not written: {message}");
                }
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

/// Write `…\Start Menu\Programs\YtmTui.lnk` pointing at the installed `ytt.exe`,
/// stamped with the `System.AppUserModel.ID` property = our AUMID. This is the
/// route the Win11 media flyout / taskbar resolve an explicit-AUMID process's
/// display name + icon from (Chrome/Spotify do the same); the HKCU key alone
/// shows "Unknown app" (docs/windows-smtc-qa-results-2026-07-04.md). Idempotent:
/// `IPersistFile::Save` overwrites. Returns the shortcut path on success.
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

    // `%APPDATA%\Microsoft\Windows\Start Menu\Programs\YtmTui.lnk`. Must live under
    // the Start Menu: Windows AppResolver only indexes Start-Menu shortcuts for
    // AUMID→identity resolution. The .lnk stem (`YtmTui`) is the displayed name.
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "APPDATA is not set".to_string())?;
    let lnk_path = std::path::PathBuf::from(appdata)
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join(format!("{DISPLAY_NAME}.lnk"));

    // Target = the installed ytt.exe (this very process, during `register`), which
    // is the media-owning exe that stamps the same explicit AUMID at startup.
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;

    // COM apartment for this call only; uninit on drop so every early return stays
    // balanced. S_FALSE ("already initialized") is still a success we must pair.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!("CoInitializeEx failed: {hr:?}"));
    }
    let _com = ComGuard;

    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("CoCreateInstance(ShellLink) failed: {e}"))?;

    // COM copies each string internally, so the wide buffer only needs to outlive
    // its own call (SetPath/SetIconLocation/Save each below).
    let exe_w = wide(&exe.to_string_lossy());
    unsafe { link.SetPath(PCWSTR(exe_w.as_ptr())) }.map_err(|e| format!("SetPath failed: {e}"))?;

    if let Some(icon) = icon {
        let icon_w = wide(&icon.to_string_lossy());
        unsafe { link.SetIconLocation(PCWSTR(icon_w.as_ptr()), 0) }
            .map_err(|e| format!("SetIconLocation failed: {e}"))?;
    }

    // Stamp System.AppUserModel.ID — the property the flyout keys off.
    // PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid 5. windows
    // 0.62 stows that const behind the heavy `Win32_Storage_EnhancedStorage`
    // feature, so build the PROPERTYKEY inline rather than pull that in.
    let store: IPropertyStore = link
        .cast()
        .map_err(|e| format!("QueryInterface(IPropertyStore) failed: {e}"))?;
    let pkey = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };
    let value = PROPVARIANT::from(APP_USER_MODEL_ID);
    unsafe { store.SetValue(&pkey, &value) }
        .map_err(|e| format!("SetValue(System.AppUserModel.ID) failed: {e}"))?;
    unsafe { store.Commit() }.map_err(|e| format!("IPropertyStore::Commit failed: {e}"))?;

    // Persist the .lnk to disk (overwrites if present).
    let persist: IPersistFile = link
        .cast()
        .map_err(|e| format!("QueryInterface(IPersistFile) failed: {e}"))?;
    let lnk_w = wide(&lnk_path.to_string_lossy());
    unsafe { persist.Save(PCWSTR(lnk_w.as_ptr()), true) }
        .map_err(|e| format!("IPersistFile::Save({}) failed: {e}", lnk_path.display()))?;

    Ok(lnk_path)
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
