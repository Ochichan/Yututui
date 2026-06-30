//! Preflight for the external tools we shell out to (`mpv`, `yt-dlp`, `ffmpeg`).
//!
//! `mpv` (playback) and `yt-dlp` (search + stream resolution) are hard runtime requirements:
//! when one is missing the startup [`missing`] check surfaces a clear, OS-specific install
//! hint in the status line instead of failing opaquely deep in an actor. `ffmpeg` is needed
//! only for download post-processing (yt-dlp merges/converts with it), so it is *not* part of
//! the startup nag — it's reported by `ytt doctor` (see `crate::doctor`) and surfaces if a
//! download needs it.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

/// Whether `bin` resolves to an executable file on `PATH`.
pub(crate) fn on_path(bin: &str) -> bool {
    let path = Path::new(bin);
    if path.is_absolute() || path.components().count() > 1 {
        return is_executable(path);
    }

    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| {
        executable_candidates(&dir, bin)
            .into_iter()
            .any(|candidate| is_executable(&candidate))
    })
}

#[cfg(not(windows))]
fn executable_candidates(dir: &Path, bin: &str) -> Vec<PathBuf> {
    vec![dir.join(bin)]
}

#[cfg(windows)]
fn executable_candidates(dir: &Path, bin: &str) -> Vec<PathBuf> {
    let base = dir.join(bin);
    if Path::new(bin).extension().is_some() {
        return vec![base];
    }

    let pathext =
        env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string().into());
    let mut candidates = vec![base];
    candidates.extend(
        pathext
            .to_string_lossy()
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| dir.join(format!("{bin}{ext}"))),
    );
    candidates
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    is_executable_metadata(&meta)
}

#[cfg(unix)]
fn is_executable_metadata(meta: &fs::Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable_metadata(_: &fs::Metadata) -> bool {
    true
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
