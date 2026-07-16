//! Win32 half of the focus ladder (`cfg(windows)` only; unsafe-inventory allowlisted).
//!
//! Two capture helpers record the hosting window at primary startup, and one resolver
//! brings it back to the foreground from a second launch. The second launch was just
//! started by the user, so it holds the interactive foreground grant —
//! `SetForegroundWindow` from here is exactly the sanctioned hand-off; when the OS still
//! refuses (foreground lock), the taskbar flash it substitutes is the accepted floor.

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows::core::BOOL;

/// The classic-conhost console window hosting this process, if any. For Windows Terminal
/// sessions this returns the hidden pseudo-console window — that's why
/// [`terminal_host_pid`] exists.
pub fn console_hwnd() -> Option<u64> {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    // SAFETY: GetConsoleWindow takes no arguments and returns null when the process has
    // no console; no pointer is dereferenced.
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as usize as u64)
    }
}

/// Walk up the parent chain (bounded) looking for the Windows Terminal host process.
/// Mirrors the `explorer_double_click_launch` sysinfo pattern in `main.rs`.
pub fn terminal_host_pid() -> Option<u32> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut system = System::new();
    let mut current = Pid::from_u32(std::process::id());
    for _ in 0..5 {
        system.refresh_processes(ProcessesToUpdate::Some(&[current]), true);
        let process = system.process(current)?;
        if process
            .name()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsTerminal.exe")
        {
            return Some(current.as_u32());
        }
        current = process.parent()?;
    }
    None
}

/// Resolve the best window for the recorded identity and bring it to the foreground.
///
/// Resolution order: a still-live recorded console HWND (classic consoles — that IS the
/// terminal window), else the first visible top-level window of the recorded Windows
/// Terminal host pid, else one owned by the player's own pid (exotic hosts). Returns
/// `false` when nothing resolved; a foreground-lock refusal after a successful resolve
/// still counts as attempted (the OS flashes the taskbar for us).
pub fn bring_to_foreground(
    console_hwnd: Option<u64>,
    terminal_host_pid: Option<u32>,
    app_pid: Option<u32>,
) -> bool {
    let target = resolve_target(console_hwnd, terminal_host_pid, app_pid);
    let Some(hwnd) = target else {
        return false;
    };
    // SAFETY: `hwnd` was validated as a live window by `resolve_target` moments ago; both
    // calls tolerate a window that died in between by returning FALSE, which we accept.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
    true
}

fn resolve_target(
    console_hwnd: Option<u64>,
    terminal_host_pid: Option<u32>,
    app_pid: Option<u32>,
) -> Option<HWND> {
    if let Some(recorded) = console_hwnd {
        let hwnd = HWND(recorded as usize as *mut core::ffi::c_void);
        // SAFETY: IsWindow/GetWindowThreadProcessId only inspect the handle table; a
        // stale or reused handle yields FALSE / pid 0 rather than UB.
        let live = unsafe {
            IsWindow(Some(hwnd)).as_bool() && {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid)) != 0 && pid != 0
            }
        };
        if live {
            return Some(hwnd);
        }
    }
    if let Some(pid) = terminal_host_pid
        && let Some(hwnd) = first_visible_window_of_pid(pid)
    {
        return Some(hwnd);
    }
    if let Some(pid) = app_pid
        && let Some(hwnd) = first_visible_window_of_pid(pid)
    {
        return Some(hwnd);
    }
    None
}

fn first_visible_window_of_pid(pid: u32) -> Option<HWND> {
    struct Search {
        pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the address of the `Search` on `first_visible_window_of_pid`'s
        // stack, which outlives the synchronous EnumWindows call that invokes this visitor.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut window_pid = 0u32;
        // SAFETY: `hwnd` is provided live by EnumWindows; the out pointer is valid for
        // the duration of the call.
        let matched = unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) != 0
                && window_pid == search.pid
                && IsWindowVisible(hwnd).as_bool()
        };
        if matched {
            search.found = Some(hwnd);
            return BOOL(0); // stop enumerating
        }
        BOOL(1)
    }
    let mut search = Search { pid, found: None };
    // SAFETY: the callback only touches `search`, which lives across this synchronous
    // call; EnumWindows reports "callback stopped early" as an error we deliberately
    // ignore (stopping early is our success path).
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}
