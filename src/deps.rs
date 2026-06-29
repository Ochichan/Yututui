//! Preflight for the external tools we shell out to (`mpv`, `yt-dlp`, `ffmpeg`).
//!
//! `mpv` (playback) and `yt-dlp` (search + stream resolution) are hard runtime requirements:
//! when one is missing the startup [`missing`] check surfaces a clear, OS-specific install
//! hint in the status line instead of failing opaquely deep in an actor. `ffmpeg` is needed
//! only for download post-processing (yt-dlp merges/converts with it), so it is *not* part of
//! the startup nag — it's reported by `ytt doctor` (see `crate::doctor`) and surfaces if a
//! download needs it.

use std::process::{Command, Stdio};

/// What an external tool is needed for. Drives whether [`missing`] nags at startup and how
/// `ytt doctor` labels it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    /// Playback / search — the app is unusable without it.
    Core,
    /// Download post-processing only (yt-dlp merge/convert).
    Downloads,
}

/// Every external tool `ytt` may invoke, paired with what it's needed for. Single source of
/// truth for both the startup preflight ([`missing`]) and `ytt doctor`.
pub const TOOLS: &[(&str, Need)] = &[
    ("mpv", Need::Core),
    ("yt-dlp", Need::Core),
    ("ffmpeg", Need::Downloads),
];

/// Whether `bin --version` runs at all (i.e. the tool is installed and on `PATH`).
pub(crate) fn on_path(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// The playback-critical tools that aren't on `PATH` (the startup preflight subset; ffmpeg is
/// download-only and intentionally excluded).
pub fn missing() -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|(_, need)| *need == Need::Core)
        .map(|(bin, _)| *bin)
        .filter(|bin| !on_path(bin))
        .collect()
}

/// A one-line, OS-appropriate install hint for the given missing tools.
pub fn install_hint(missing: &[&str]) -> String {
    let tools = missing.join(" ");
    // Tool names and the brew/scoop commands stay verbatim in both languages; only the
    // surrounding guidance is localized.
    if crate::i18n::is_korean() {
        if cfg!(target_os = "macos") {
            format!("{tools} 없음 — 설치: brew install {tools}")
        } else if cfg!(target_os = "windows") {
            format!("{tools} 없음 — 설치: scoop install {tools}  (또는 winget)")
        } else {
            format!("{tools} 없음 — 패키지 매니저로 설치하세요")
        }
    } else if cfg!(target_os = "macos") {
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

    #[test]
    fn ffmpeg_is_download_only_and_never_a_startup_requirement() {
        // ffmpeg is a known tool tagged download-only…
        let need = TOOLS.iter().find(|(b, _)| *b == "ffmpeg").map(|(_, n)| *n);
        assert_eq!(need, Some(Need::Downloads));
        // …so it can never appear in the startup preflight, which is Core-only —
        // regardless of whether ffmpeg happens to be installed on this machine.
        assert!(!missing().contains(&"ffmpeg"));
    }
}
