//! Startup preflight for the external tools we shell out to (`mpv`, `yt-dlp`).
//!
//! Both are hard runtime requirements — mpv is the playback engine, yt-dlp does search
//! and stream resolution. When one is missing we surface a clear, OS-specific install
//! hint in the status line instead of failing opaquely deep in an actor.

use std::process::{Command, Stdio};

/// Whether `bin --version` runs at all (i.e. the tool is installed and on `PATH`).
fn on_path(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// The required tools that aren't on `PATH`.
pub fn missing() -> Vec<&'static str> {
    ["mpv", "yt-dlp"].into_iter().filter(|b| !on_path(b)).collect()
}

/// A one-line, OS-appropriate install hint for the given missing tools.
pub fn install_hint(missing: &[&str]) -> String {
    let tools = missing.join(" ");
    if cfg!(target_os = "macos") {
        format!("Missing {tools} — install with: brew install {tools}")
    } else if cfg!(target_os = "windows") {
        format!("Missing {tools} — install with: scoop install {tools}  (or winget)")
    } else {
        format!("Missing {tools} — install via your package manager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_names_each_missing_tool() {
        let h = install_hint(&["mpv", "yt-dlp"]);
        assert!(h.contains("mpv"));
        assert!(h.contains("yt-dlp"));
    }

    #[test]
    fn nonexistent_binary_is_not_on_path() {
        assert!(!on_path("ytm-tui-definitely-not-a-real-binary-xyzzy"));
    }
}
