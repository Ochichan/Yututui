//! Small native-shell clipboard adapter for WebView operations.
//!
//! The desktop targets already ship an OS clipboard utility. Invoke it directly with a piped
//! stdin (never a shell command), keeping clipboard support dependency- and lockfile-neutral.

use std::io;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::io::Write;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::{Command, Stdio};

pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn copy_text(text: &str) -> io::Result<()> {
    validate_text(text)?;
    let mut command = clipboard_command();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("clipboard process has no stdin"))?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "clipboard helper exited with {status}"
        )))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn copy_text(text: &str) -> io::Result<()> {
    validate_text(text)?;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard is unavailable on this desktop target",
    ))
}

fn validate_text(text: &str) -> io::Result<()> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard text exceeds the desktop IPC limit",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn clipboard_command() -> Command {
    Command::new("/usr/bin/pbcopy")
}

#[cfg(target_os = "windows")]
fn clipboard_command() -> Command {
    Command::new("clip.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_clipboard_payload_is_rejected_before_process_launch() {
        let text = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert_eq!(
            copy_text(&text).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
