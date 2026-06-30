//! `ytt doctor` — a one-shot environment diagnostic.
//!
//! Reports whether each external tool (`mpv`/`yt-dlp`/`ffmpeg`) is on `PATH`, whether the
//! download and data directories are writable, and on Linux whether the optional
//! open-in-browser / clipboard helpers exist — each with an OS- and language-appropriate
//! hint. Runs in the synchronous `main` path *before* any terminal setup, so it never touches
//! raw mode or the alternate screen. Returns a process exit code: non-zero if a
//! playback-critical tool or a required directory is unusable; zero otherwise (download-only
//! ffmpeg and the Linux helpers are warnings, not failures).

use crate::deps::{self, Need};
use crate::{config, i18n};
use std::path::{Path, PathBuf};

/// Run the diagnostic, printing a report, and return the process exit code.
pub fn run() -> i32 {
    // Localize using the saved UI language, exactly as the TUI does at startup.
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    // `ok` flips to false only on a problem that actually stops the app working
    // (a Core tool missing, or a required directory not writable).
    let mut ok = true;

    println!("ytt doctor — ytm-tui {}", env!("CARGO_PKG_VERSION"));
    println!();

    // 1) External tools.
    println!(
        "{}",
        if kr {
            "외부 도구"
        } else {
            "External tools"
        }
    );
    for &(bin, need) in deps::TOOLS {
        let role = tool_role(bin, kr);
        if deps::on_path(bin) {
            println!("  ✓ {bin:<8} ({role})");
        } else {
            // `install_hint` is already OS- and language-aware and accepts any tool name.
            println!("  ✗ {bin:<8} ({role}) — {}", deps::install_hint(&[bin]));
            // A missing playback-critical tool makes the app unusable; ffmpeg only blocks downloads.
            if need == Need::Core {
                ok = false;
            }
        }
    }
    println!();

    // 2) Directories the app needs to write into.
    println!("{}", if kr { "디렉터리" } else { "Directories" });
    ok &= report_dir(
        if kr { "다운로드" } else { "downloads" },
        &cfg.effective_download_dir(),
        kr,
    );
    if let Some(data) = data_dir() {
        ok &= report_dir(if kr { "데이터" } else { "data" }, &data, kr);
    }
    println!();

    // 3) Linux-only optional helpers (open-in-browser, clipboard). Informational: their
    //    absence degrades two niceties but never stops playback, so it doesn't fail `doctor`.
    #[cfg(target_os = "linux")]
    {
        println!(
            "{}",
            if kr {
                "리눅스 도우미 (선택)"
            } else {
                "Linux helpers (optional)"
            }
        );
        let mark = |present: bool| if present { "✓" } else { "✗" };
        println!("  {} xdg-open", mark(deps::on_path("xdg-open")));
        let clip = ["wl-copy", "xclip", "xsel"]
            .into_iter()
            .find(|c| deps::on_path(c));
        let clip_label = if kr { "클립보드" } else { "clipboard" };
        match clip {
            Some(found) => println!("  ✓ {clip_label} ({found})"),
            None => println!("  ✗ {clip_label} (wl-copy/xclip/xsel)"),
        }
        println!();
    }

    // Result line + exit code.
    if ok {
        println!(
            "{}",
            if kr {
                "정상: 필수 도구와 디렉터리가 모두 준비되었습니다."
            } else {
                "OK: all required tools and directories are ready."
            }
        );
        0
    } else {
        println!(
            "{}",
            if kr {
                "문제 발견: 위의 ✗ 항목을 설치하거나 수정하세요."
            } else {
                "Problems found: install or fix the ✗ items above."
            }
        );
        1
    }
}

/// A short, localized description of what a tool is for.
fn tool_role(bin: &str, kr: bool) -> &'static str {
    match (bin, kr) {
        ("mpv", false) => "playback",
        ("mpv", true) => "재생",
        ("yt-dlp", false) => "search & streaming",
        ("yt-dlp", true) => "검색·스트리밍",
        ("ffmpeg", false) => "downloads",
        ("ffmpeg", true) => "다운로드",
        (_, false) => "external tool",
        (_, true) => "외부 도구",
    }
}

/// The per-user data directory, mirroring `config.rs`'s `ProjectDirs::from("", "", "ytm-tui")`.
fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().to_path_buf())
}

/// Print one directory line and return whether it's usable.
fn report_dir(label: &str, dir: &Path, kr: bool) -> bool {
    if dir_is_writable(dir) {
        println!("  ✓ {label} — {}", dir.display());
        true
    } else {
        let note = if kr { "쓰기 불가" } else { "not writable" };
        println!("  ✗ {label} — {} ({note})", dir.display());
        false
    }
}

/// Whether the app could write into `dir` (creating it on demand). Diagnostic-pure: it never
/// creates the target tree itself — it walks up to the nearest existing ancestor and probes a
/// throwaway file there, since a writable ancestor means `create_dir_all` would later succeed.
fn dir_is_writable(dir: &Path) -> bool {
    let mut anchor = dir;
    while !anchor.exists() {
        match anchor.parent() {
            Some(parent) => anchor = parent,
            None => return false,
        }
    }
    let probe = anchor.join(".ytt-doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_tool_has_a_localized_role() {
        // Both languages must yield a non-empty, non-fallback label for each real tool.
        for &(bin, _) in deps::TOOLS {
            for kr in [false, true] {
                let role = tool_role(bin, kr);
                assert!(!role.is_empty());
                assert_ne!(role, if kr { "외부 도구" } else { "external tool" });
            }
        }
    }

    #[test]
    fn an_existing_writable_dir_is_reported_writable() {
        assert!(dir_is_writable(&std::env::temp_dir()));
    }

    #[test]
    fn a_missing_dir_under_a_writable_parent_is_writable() {
        // The app creates these on demand, so "doesn't exist yet" must still read as usable.
        let nested = std::env::temp_dir().join("ytt-doctor-nonexistent-xyzzy/sub/dir");
        assert!(!nested.exists());
        assert!(dir_is_writable(&nested));
        // The probe must not have created the target tree.
        assert!(!nested.exists());
    }
}
