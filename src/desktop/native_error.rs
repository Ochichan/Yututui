//! Last-resort native error presentation for failures that happen before a WebView exists.

/// Show a blocking native error dialog where the platform offers one. This is reserved for
/// fatal desktop-shell failures (single-instance endpoint, event loop, or WebView creation),
/// not routine player command errors, which belong in the correlated panel/main UI.
pub fn show(title: &str, message: &str) {
    #[cfg(windows)]
    show_windows(title, message);
    #[cfg(target_os = "macos")]
    show_macos(title, message);
    #[cfg(all(not(windows), not(target_os = "macos")))]
    eprintln!("{title}: {message}");
}

/// Show a non-blocking native toast for a recoverable command failure when no WebView owns a
/// correlated result surface. Fatal startup failures still use [`show`].
pub fn notify(title: &str, message: &str) {
    crate::notify::emit_native_notification(title, message);
}

#[cfg(windows)]
fn show_windows(title: &str, message: &str) {
    let title = title.encode_utf16().chain([0]).collect::<Vec<_>>();
    let message = message.encode_utf16().chain([0]).collect::<Vec<_>>();
    crate::desktop::platform::windows::show_native_error(&title, &message);
}

#[cfg(target_os = "macos")]
fn show_macos(title: &str, message: &str) {
    use std::process::Command;

    // Passing user-controlled text through argv keeps it out of the AppleScript source.
    // `display alert` is a native AppKit dialog and works before our accessory event loop starts.
    let status = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display alert (item 1 of argv) message (item 2 of argv) as critical",
            "-e",
            "end run",
            "--",
            title,
            message,
        ])
        .status();
    if !status.is_ok_and(|status| status.success()) {
        eprintln!("{title}: {message}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_linked_without_showing_a_dialog() {
        let show: fn(&str, &str) = super::show;
        let notify: fn(&str, &str) = super::notify;
        let _ = show;
        let _ = notify;
    }
}
