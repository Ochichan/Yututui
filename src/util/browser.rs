//! Opening URLs in the system browser, shared by the TUI (About card, account
//! connect flows) and one-shot CLI commands (`ytt auth …`).

use std::process::Stdio;

use crate::util::process;

/// Open `url` in the system's default browser, fire-and-forget. Spawns the platform opener
/// (`open` / `xdg-open` / `cmd start`) detached with stdio nulled so it can't touch the TUI's
/// terminal; any failure (no opener installed) is ignored — callers always show the URL too.
pub fn open_in_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = process::std_command("open", process::ProcessProfile::DesktopOpen);
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin; the empty "" is its (ignored) window-title argument.
        let mut c = process::std_command("cmd", process::ProcessProfile::DesktopOpen);
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = process::std_command("xdg-open", process::ProcessProfile::DesktopOpen);
        c.arg(url);
        c
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
