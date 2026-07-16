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
    resolve_on_path(bin).is_some()
}

/// The absolute path `bin` resolves to on `PATH`, if any — the same lookup the OS
/// would do at spawn. Used by `crate::tools` to identify the system yt-dlp so its
/// version can be probed (and probe-cached) against the managed copy.
pub(crate) fn resolve_on_path(bin: &str) -> Option<PathBuf> {
    resolve_all_on_path(bin).into_iter().next()
}

/// All executable paths `bin` resolves to on `PATH`, in the exact PATH order the
/// OS lookup would consider them. Diagnostic code uses this to show shadowed
/// candidates such as Scoop shims and Python Scripts entries.
pub(crate) fn resolve_all_on_path(bin: &str) -> Vec<PathBuf> {
    let Some(paths) = env::var_os("PATH") else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in env::split_paths(&paths) {
        for candidate in executable_candidates(&dir, bin) {
            if is_executable(&candidate)
                && !found.iter().any(|p| same_path(p.as_path(), &candidate))
            {
                found.push(candidate);
            }
        }
    }
    found
}

fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
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

/// The playback-critical tools that aren't usable (the startup preflight subset; ffmpeg is
/// download-only and intentionally excluded). Runs after `tools::init`, so a managed or
/// override yt-dlp (not on `PATH`) and a configured mpv path both count as present; before
/// init it degrades to the plain PATH check.
pub fn missing() -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|(_, need)| *need == Need::Core)
        .map(|(bin, _)| *bin)
        .filter(|bin| match *bin {
            "yt-dlp" => {
                crate::tools::ytdlp_selection_error().is_some()
                    || (crate::tools::ytdlp_selection().is_none() && !on_path(bin))
            }
            "mpv" => !on_path(&crate::tools::mpv_program()),
            other => !on_path(other),
        })
        .collect()
}

/// Tools required before handing a download to yt-dlp. mpv is playback-only; yt-dlp and
/// ffmpeg are both part of the download pipeline.
pub fn missing_for_downloads() -> Vec<&'static str> {
    ["yt-dlp", "ffmpeg"]
        .into_iter()
        .filter(|bin| match *bin {
            "yt-dlp" => {
                crate::tools::ytdlp_selection_error().is_some()
                    || (crate::tools::ytdlp_selection().is_none() && !on_path(bin))
            }
            other => !on_path(other),
        })
        .collect()
}

/// A copyable, platform-appropriate command assembled only from the closed tool-name set above.
pub fn install_command(missing: &[&str]) -> Option<String> {
    let tools = missing
        .iter()
        .copied()
        .filter(|tool| TOOLS.iter().any(|(known, _)| known == tool))
        .collect::<Vec<_>>();
    if tools.is_empty() {
        return None;
    }
    if cfg!(target_os = "windows") {
        let prefix = if tools.contains(&"mpv") {
            "scoop bucket add extras; "
        } else {
            ""
        };
        Some(format!("{prefix}scoop install {}", tools.join(" ")))
    } else if cfg!(target_os = "macos") {
        Some(format!("brew install {}", tools.join(" ")))
    } else if on_path("apt") || on_path("apt-get") {
        Some(format!("sudo apt install {}", tools.join(" ")))
    } else if on_path("pacman") {
        Some(format!("sudo pacman -S {}", tools.join(" ")))
    } else {
        None
    }
}

pub fn setup_guide_url() -> &'static str {
    match crate::i18n::current() {
        crate::i18n::Language::Korean => {
            "https://github.com/Ochichan/Yututui/blob/main/README.ko.md#재생-도구"
        }
        crate::i18n::Language::Japanese => {
            "https://github.com/Ochichan/Yututui/blob/main/README.ja.md#再生ツール"
        }
        _ => "https://github.com/Ochichan/Yututui/blob/main/README.md#runtime-tools",
    }
}

/// A one-line, OS-appropriate install hint for the given missing tools.
pub fn install_hint(missing: &[&str]) -> String {
    let tools = missing.join(" ");
    // yt-dlp has a package-manager-free path: the app can fetch its own copy.
    let ytdlp_alt = missing.contains(&"yt-dlp");
    // Tool names and the brew/scoop commands stay verbatim in both languages; only the
    // surrounding guidance is localized.
    let mut hint = match crate::i18n::current() {
        crate::i18n::Language::Korean => {
            if cfg!(target_os = "macos") {
                format!("{tools} 없음 — 설치: brew install {tools}")
            } else if cfg!(target_os = "windows") {
                format!("{tools} 없음 — 설치: scoop install {tools}  (또는 winget)")
            } else {
                format!("{tools} 없음 — 패키지 매니저로 설치하세요")
            }
        }
        crate::i18n::Language::Japanese => {
            if cfg!(target_os = "macos") {
                format!("{tools} がありません — インストール: brew install {tools}")
            } else if cfg!(target_os = "windows") {
                format!(
                    "{tools} がありません — インストール: scoop install {tools}  (または winget)"
                )
            } else {
                format!("{tools} がありません — パッケージマネージャーでインストールしてください")
            }
        }
        _ => {
            if cfg!(target_os = "macos") {
                format!("Missing {tools} — install with: brew install {tools}")
            } else if cfg!(target_os = "windows") {
                format!("Missing {tools} — install with: scoop install {tools}  (or winget)")
            } else {
                format!("Missing {tools} — install via your package manager")
            }
        }
    };
    if ytdlp_alt {
        hint.push_str(match crate::i18n::current() {
            crate::i18n::Language::Korean => "  (yt-dlp는 `ytt tools update`로도 받을 수 있어요)",
            crate::i18n::Language::Japanese => "  (yt-dlpは`ytt tools update`でも取得できます)",
            _ => "  (yt-dlp: `ytt tools update` also works)",
        });
    }
    hint
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
        assert!(!on_path("yututui-definitely-not-a-real-binary-xyzzy"));
    }

    #[test]
    fn install_command_rejects_unknown_tool_names() {
        assert_eq!(install_command(&["not-a-tool"]), None);
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
