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
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const KONSOLE_SIXEL_TUI_MIN_VERSION: u32 = 260_400;

#[path = "doctor/audio_report.rs"]
mod audio_report;
#[path = "doctor/directory_probe.rs"]
mod directory_probe;
#[path = "doctor/long_form_seek.rs"]
mod long_form_seek;
use audio_report::{mpv_lifetime_report, run as run_audio};
#[cfg(test)]
use directory_probe::dir_is_writable;
use directory_probe::report_dir;

/// Run the diagnostic, printing a report, and return the process exit code.
pub fn run() -> i32 {
    run_inner(false)
}

pub fn run_with_args(args: &[String]) -> i32 {
    if matches!(args, [cmd, flag] if cmd == "terminal" && flag == "--json") {
        return run_terminal_json();
    }
    if matches!(args, [cmd, flag] if cmd == "terminal" && (flag == "--help" || flag == "-h")) {
        println!("Usage: ytt doctor terminal --json");
        return 0;
    }
    if matches!(args, [cmd] if cmd == "privacy") {
        return run_privacy(false);
    }
    if matches!(args, [cmd, flag] if cmd == "privacy" && flag == "--cleanup") {
        return run_privacy(true);
    }
    if matches!(args, [cmd, flag] if cmd == "privacy" && (flag == "--help" || flag == "-h")) {
        println!("Usage: ytt doctor privacy [--cleanup]");
        println!("       Report secret-bearing files and recovery backups");
        return 0;
    }
    if matches!(args, [cmd] if cmd == "audio") {
        return run_audio(false);
    }
    if matches!(args, [cmd, flag] if cmd == "audio" && (flag == "--verbose" || flag == "-v")) {
        return run_audio(true);
    }
    if matches!(args, [cmd, flag] if cmd == "audio" && (flag == "--help" || flag == "-h")) {
        println!("Usage: ytt doctor audio [--verbose]");
        println!("       Report the active audio backend, mpv settings, and capabilities");
        println!("       Note: mpv output/device/cache/extra_args apply on the next player launch");
        println!(
            "       Config escape hatch: audio.mpv.extra_args (no settings UI; config file only)"
        );
        return 0;
    }
    let verbose = match args {
        [] => false,
        [arg] if arg == "--verbose" || arg == "-v" => true,
        [arg] if arg == "--help" || arg == "-h" => {
            println!("Usage: ytt doctor [--verbose]");
            println!("       ytt doctor audio [--verbose]");
            println!("       ytt doctor privacy [--cleanup]");
            println!("       ytt doctor terminal --json");
            return 0;
        }
        _ => {
            eprintln!("usage: ytt doctor [--verbose]");
            eprintln!("       ytt doctor audio [--verbose]");
            eprintln!("       ytt doctor privacy [--cleanup]");
            eprintln!("       ytt doctor terminal --json");
            return 2;
        }
    };
    run_inner(verbose)
}

fn init_tools_sync(cfg: &config::Config) {
    // Resolve the yt-dlp/mpv selection exactly as the app would (doctor runs in the
    // synchronous main path, so block on a throwaway current-thread runtime).
    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        rt.block_on(crate::tools::init(&cfg.tools));
    }
}

#[derive(Serialize)]
struct TerminalDoctor {
    term: Option<String>,
    term_program: Option<String>,
    wt_session: bool,
    image_protocol: &'static str,
    image_protocol_source: &'static str,
    native_image_hint: bool,
    image_probe_timeout_ms: u64,
    image_protocol_override: Option<String>,
    image_protocol_override_supported: Option<bool>,
    image_protocol_override_suggestions: Vec<&'static str>,
    zoom_mode: &'static str,
    zoom_mode_source: &'static str,
    keyboard_enhancement_supported: Option<bool>,
    mouse_capture_configured: bool,
    stdout_is_tty: bool,
    stdin_is_tty: bool,
    warnings: Vec<String>,
}

fn run_terminal_json() -> i32 {
    // No config load, cookie read, playback init, mpv spawn, terminal raw mode, or writes.
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let konsole_version = std::env::var("KONSOLE_VERSION").ok();
    let wt_session = std::env::var_os("WT_SESSION").is_some();
    let image_protocol_override = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok();
    let image_protocol_override_supported = image_protocol_override
        .as_deref()
        .map(image_protocol_override_supported);
    let image_protocol = terminal_image_protocol(
        term.as_deref(),
        term_program.as_deref(),
        wt_session,
        konsole_version.as_deref(),
    );
    let native_image_hint =
        terminal_native_image_hint(term.as_deref(), term_program.as_deref(), wt_session);
    let image_protocol_override_suggestions =
        terminal_image_override_suggestions(term.as_deref(), term_program.as_deref(), wt_session);
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let mut warnings = Vec::new();
    if !stdout_is_tty {
        warnings.push("stdout is not a TTY; YuTuTui! will skip native image probing".to_string());
    }
    if !stdin_is_tty {
        warnings.push("stdin is not a TTY; interactive terminal probes may not answer".to_string());
    }
    if image_protocol_override_supported == Some(false) {
        warnings.push(
            "unsupported YTM_TUI_IMAGE_PROTOCOL; accepted values are halfblocks, sixel, kitty, iterm2"
                .to_string(),
        );
    }
    if native_image_hint
        && matches!(image_protocol, "unknown" | "halfblocks_or_retro")
        && !image_protocol_override_suggestions.is_empty()
    {
        warnings.push(format!(
            "native image hint detected; if album art falls back to halfblocks, try {}",
            image_protocol_override_suggestions.join(", ")
        ));
    }

    let report = TerminalDoctor {
        image_protocol,
        image_protocol_source: "environment",
        native_image_hint,
        image_probe_timeout_ms: terminal_image_probe_timeout_ms(native_image_hint),
        image_protocol_override,
        image_protocol_override_supported,
        image_protocol_override_suggestions,
        zoom_mode: terminal_zoom_mode(term.as_deref(), term_program.as_deref(), wt_session),
        zoom_mode_source: "environment",
        keyboard_enhancement_supported: terminal_keyboard_hint(
            term.as_deref(),
            term_program.as_deref(),
            wt_session,
        ),
        mouse_capture_configured: true,
        term,
        term_program,
        wt_session,
        stdout_is_tty,
        stdin_is_tty,
        warnings,
    };

    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(e) => {
            eprintln!("ytt doctor: could not encode terminal report: {e}");
            1
        }
    }
}

struct SecretFile {
    label: &'static str,
    note: &'static str,
    path: PathBuf,
    cleanup_managed_backups: bool,
}

fn run_privacy(cleanup: bool) -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();
    let files = secret_files(&cfg);
    let mut ok = true;
    let mut removed_total = 0usize;

    if cleanup {
        for file in files.iter().filter(|file| file.cleanup_managed_backups) {
            match crate::util::safe_fs::enforce_secret_backup_retention(&file.path) {
                Ok(removed) => removed_total += removed,
                Err(e) => {
                    ok = false;
                    eprintln!(
                        "{} {}: {e}",
                        if kr {
                            "개인정보 백업 정리 실패:"
                        } else {
                            "privacy backup cleanup failed for"
                        },
                        privacy_path(&file.path)
                    );
                }
            }
        }
    }

    println!(
        "{}",
        if kr {
            "개인정보 파일"
        } else {
            "Privacy-sensitive files"
        }
    );
    if cleanup {
        println!(
            "  {}",
            if kr {
                format!("정리됨: 오래된 secret recovery backup {removed_total}개 제거")
            } else {
                format!("cleanup: removed {removed_total} old secret recovery backups")
            }
        );
    }

    let mut over_retention = false;
    for file in &files {
        let exists = file.path.exists();
        println!(
            "  {} {} — {}",
            if exists { "✓" } else { "-" },
            file.label,
            privacy_path(&file.path)
        );
        println!("    {}", file.note);
        match crate::util::safe_fs::recovery_backups(&file.path) {
            Ok(backups) => {
                if file.cleanup_managed_backups
                    && backups.len() > crate::util::safe_fs::SECRET_BACKUP_RETENTION
                {
                    over_retention = true;
                }
                println!("    {}", backup_summary(&backups, kr));
            }
            Err(e) => {
                ok = false;
                println!(
                    "    {}: {e}",
                    if kr {
                        "백업을 확인할 수 없음"
                    } else {
                        "could not inspect backups"
                    }
                );
            }
        }
    }

    if over_retention && !cleanup {
        println!(
            "{}",
            if kr {
                format!(
                    "`ytt doctor privacy --cleanup`으로 secret recovery backup을 최근 {}개만 남길 수 있어요.",
                    crate::util::safe_fs::SECRET_BACKUP_RETENTION
                )
            } else {
                format!(
                    "Run `ytt doctor privacy --cleanup` to keep only the newest {} secret recovery backups.",
                    crate::util::safe_fs::SECRET_BACKUP_RETENTION
                )
            }
        );
    }

    if ok { 0 } else { 1 }
}

fn secret_files(cfg: &config::Config) -> Vec<SecretFile> {
    let mut files = Vec::new();
    if let Some(path) = config::config_path() {
        push_secret_file(
            &mut files,
            SecretFile {
                label: "config.json",
                note: "May contain YouTube cookies, Gemini keys, and scrobble tokens.",
                path,
                cleanup_managed_backups: true,
            },
        );
    }
    if let Some(path) = cfg.effective_cookies_file() {
        push_secret_file(
            &mut files,
            SecretFile {
                label: "cookies.txt",
                note: "Browser-exported cookies used for YouTube Music auth.",
                path,
                cleanup_managed_backups: false,
            },
        );
    }
    if let Some(data) = data_dir() {
        push_secret_file(
            &mut files,
            SecretFile {
                label: "cookies.external.txt",
                note: "Private imported cookies copy handed to mpv/yt-dlp.",
                path: data.join(config::EXTERNAL_COOKIES_COPY),
                cleanup_managed_backups: true,
            },
        );
    }
    if let Some(path) = crate::spotify::auth::token_path() {
        push_secret_file(
            &mut files,
            SecretFile {
                label: "spotify_token.json",
                note: "Spotify OAuth access and refresh tokens.",
                path,
                cleanup_managed_backups: true,
            },
        );
    }
    files
}

fn push_secret_file(files: &mut Vec<SecretFile>, file: SecretFile) {
    if !files.iter().any(|existing| existing.path == file.path) {
        files.push(file);
    }
}

fn backup_summary(backups: &[crate::util::safe_fs::RecoveryBackup], kr: bool) -> String {
    if backups.is_empty() {
        return if kr {
            "recovery backup: 0개".to_owned()
        } else {
            "recovery backups: 0".to_owned()
        };
    }
    let newest = backups
        .iter()
        .filter_map(|backup| backup.modified_unix)
        .max()
        .map(age_label)
        .unwrap_or_else(|| {
            if kr {
                "나이 알 수 없음".to_owned()
            } else {
                "unknown age".to_owned()
            }
        });
    let bytes: u64 = backups.iter().map(|backup| backup.len).sum();
    if kr {
        format!(
            "recovery backup: {}개, 총 {} bytes, 최근 {}",
            backups.len(),
            bytes,
            newest
        )
    } else {
        format!(
            "recovery backups: {}, {} bytes total, newest {}",
            backups.len(),
            bytes,
            newest
        )
    }
}

fn age_label(modified_unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
        .unwrap_or(modified_unix);
    let secs = now.saturating_sub(modified_unix);
    if secs < 120 {
        "just now".to_owned()
    } else if secs < 7200 {
        format!("{} min ago", secs / 60)
    } else if secs < 172_800 {
        format!("{} h ago", secs / 3600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

fn privacy_path(path: &Path) -> String {
    if let Some(base) = directories::BaseDirs::new()
        && let Ok(stripped) = path.strip_prefix(base.home_dir())
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

fn terminal_native_image_hint(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> bool {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    wt_session
        || env_nonempty("KITTY_WINDOW_ID")
        || env_nonempty("WEZTERM_EXECUTABLE")
        || env_nonempty("KONSOLE_VERSION")
        || term_program == "iterm.app"
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
        || [
            "kitty", "ghostty", "wezterm", "foot", "konsole", "mlterm", "mintty", "rio", "contour",
        ]
        .iter()
        .any(|hint| term.contains(hint))
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn terminal_image_probe_timeout_ms(native_image_hint: bool) -> u64 {
    if native_image_hint { 700 } else { 250 }
}

fn image_protocol_override_supported(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "halfblocks" | "halfblock" | "blocks" | "block" | "sixel" | "kitty" | "iterm2" | "iterm"
    )
}

fn terminal_image_override_suggestions(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> Vec<&'static str> {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();

    if env_nonempty("KITTY_WINDOW_ID")
        || term.contains("kitty")
        || term.contains("ghostty")
        || term_program.contains("ghostty")
    {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=kitty"];
    }
    if term_program == "iterm.app" {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=iterm2"];
    }
    if env_nonempty("WEZTERM_EXECUTABLE")
        || term_program.contains("wezterm")
        || term.contains("wezterm")
    {
        return vec![
            "YTM_TUI_IMAGE_PROTOCOL=iterm2",
            "YTM_TUI_IMAGE_PROTOCOL=kitty",
            "YTM_TUI_IMAGE_PROTOCOL=sixel",
        ];
    }
    if env_nonempty("KONSOLE_VERSION") || term.contains("konsole") {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"];
    }
    if wt_session || term.contains("foot") || term.contains("mintty") || term.contains("mlterm") {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"];
    }
    if terminal_native_image_hint(Some(&term), Some(&term_program), wt_session) {
        return vec![
            "YTM_TUI_IMAGE_PROTOCOL=kitty",
            "YTM_TUI_IMAGE_PROTOCOL=iterm2",
            "YTM_TUI_IMAGE_PROTOCOL=sixel",
        ];
    }
    Vec::new()
}

fn terminal_image_protocol(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
    konsole_version: Option<&str>,
) -> &'static str {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if term.contains("kitty") {
        "kitty"
    } else if term_program == "iterm.app" {
        "iterm2"
    } else if term_program.contains("wezterm") {
        "iterm2_or_kitty_or_sixel"
    } else if wt_session {
        "sixel_versioned"
    } else if term.contains("foot") || term.contains("mintty") || term.contains("mlterm") {
        "sixel"
    } else if term.contains("konsole") || konsole_version.is_some_and(|version| !version.is_empty())
    {
        if konsole_version
            .and_then(|version| version.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= KONSOLE_SIXEL_TUI_MIN_VERSION)
        {
            "sixel_versioned"
        } else {
            "halfblocks"
        }
    } else if term_program.contains("ghostty") || term.contains("ghostty") {
        "kitty"
    } else if term.contains("linux") {
        "halfblocks_or_retro"
    } else {
        "unknown"
    }
}

fn terminal_zoom_mode(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> &'static str {
    if let Ok(value) = std::env::var("YTM_TUI_TEXT_SIZING") {
        return match value.as_str() {
            "0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => "none_forced",
            "dhl" | "DHL" | "decdhl" => "decdhl_forced",
            _ => "probe_requested",
        };
    }
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if term.contains("kitty") {
        "osc66_versioned"
    } else if wt_session {
        "decdhl_expected"
    } else if term_program.contains("wezterm") || term_program.contains("ghostty") {
        "unknown_probe_required"
    } else {
        "unknown"
    }
}

fn terminal_keyboard_hint(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> Option<bool> {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if term.contains("kitty")
        || term.contains("foot")
        || term.contains("alacritty")
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
        || wt_session
    {
        Some(true)
    } else {
        None
    }
}

fn run_inner(verbose: bool) -> i32 {
    // Localize using the saved UI language, exactly as the TUI does at startup.
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    init_tools_sync(&cfg);

    // `ok` flips to false only on a problem that actually stops the app working
    // (a Core tool missing, or a required directory not writable).
    let mut ok = true;

    println!("ytt doctor — YuTuTui! {}", env!("CARGO_PKG_VERSION"));
    // Install method (from the running binary's path) + any cached "newer release" notice.
    // Offline: reads only persisted state, never the network — run `ytt update` to re-check.
    let method = crate::update::detect_install_method();
    match crate::update::cached_newer_tag() {
        Some(latest) => {
            let display = latest.trim_start_matches(['v', 'V']);
            println!(
                "{} {} · {}",
                if kr {
                    "설치 방식:"
                } else {
                    "installed via:"
                },
                method.label(),
                if kr {
                    format!("새 버전 v{display} 사용 가능 (`ytt update`)")
                } else {
                    format!("update available: v{display} (`ytt update`)")
                }
            );
        }
        None => println!(
            "{} {}",
            if kr {
                "설치 방식:"
            } else {
                "installed via:"
            },
            method.label()
        ),
    }
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
        match bin {
            // yt-dlp reports the *selection* (managed/system/override), not bare PATH
            // presence — the managed binary lives outside PATH by design.
            "yt-dlp" => {
                if let Some(sel) = crate::tools::ytdlp_selection() {
                    println!(
                        "  ✓ {bin:<8} ({role}) — {} {} · {}",
                        sel.source.label(),
                        sel.version.as_deref().unwrap_or("?"),
                        sel.path.display()
                    );
                } else if let Some(error) = crate::tools::ytdlp_selection_error() {
                    println!("  ✗ {bin:<8} ({role}) — {error}");
                    ok = false;
                } else {
                    println!("  ✗ {bin:<8} ({role}) — {}", deps::install_hint(&[bin]));
                    ok = false;
                }
            }
            // mpv honors the YTM_MPV / tools.mpv_path override.
            "mpv" => {
                let program = crate::tools::mpv_program();
                if deps::on_path(&program) {
                    match crate::player::mpv::ensure_lifeline_supported() {
                        Ok(()) => {
                            let selected = if program == "mpv" {
                                String::new()
                            } else {
                                format!(" · {program}")
                            };
                            println!(
                                "  ✓ {bin:<8} ({role}) — {}{selected}",
                                mpv_lifetime_report(true, None, kr)
                            );
                        }
                        Err(error) => {
                            println!("  ✗ {bin:<8} ({role}) — {error:#}");
                            ok = false;
                        }
                    }
                } else {
                    println!("  ✗ {bin:<8} ({role}) — {}", deps::install_hint(&[bin]));
                    ok = false;
                }
            }
            _ => {
                if deps::on_path(bin) {
                    println!("  ✓ {bin:<8} ({role})");
                } else {
                    // `install_hint` is OS- and language-aware and accepts any tool name.
                    println!("  ✗ {bin:<8} ({role}) — {}", deps::install_hint(&[bin]));
                    // A missing playback-critical tool makes the app unusable; ffmpeg
                    // only blocks downloads.
                    if need == Need::Core {
                        ok = false;
                    }
                }
            }
        }
    }
    println!();

    // 1b) Managed yt-dlp status (the auto-updated copy in <data>/tools).
    print_managed_ytdlp(&cfg, kr);
    if verbose {
        print_ytdlp_verbose(&cfg);
    }

    // 1c) Modern yt-dlp needs a supported JS runtime for YouTube nsig solving (deno is auto-used;
    // node/bun/quickjs are wired via --js-runtimes). Soft-warn if none is usable — playback still
    // partially works via the tv-client fallback, so this doesn't fail the doctor.
    let js = crate::tools::js_runtime_diagnostics();
    if let Some(probe) = js.iter().find(|probe| probe.supported) {
        let version = probe
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let rt = probe.runtime;
        println!(
            "{}",
            if kr {
                if rt.flag_value().is_none() {
                    format!("JS 런타임: ✓ {}{} (자동 사용)", rt.label(), version)
                } else {
                    format!(
                        "JS 런타임: ✓ {}{} (--js-runtimes 로 연결)",
                        rt.label(),
                        version
                    )
                }
            } else if rt.flag_value().is_none() {
                format!("JS runtime: ✓ {}{} (auto-used)", rt.label(), version)
            } else {
                format!(
                    "JS runtime: ✓ {}{} (wired via --js-runtimes)",
                    rt.label(),
                    version
                )
            }
        );
    } else if let Some(probe) = js.first() {
        let version = probe
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let reason = probe.reason.unwrap_or("unsupported version");
        println!(
            "{}",
            if kr {
                format!(
                    "JS 런타임: ✗ {}{} 미지원 — {reason}; `deno` 설치를 권장해요.",
                    probe.runtime.label(),
                    version
                )
            } else {
                format!(
                    "JS runtime: ✗ {}{} unsupported — {reason}; install `deno`.",
                    probe.runtime.label(),
                    version
                )
            }
        );
    } else {
        println!(
            "{}",
            if kr {
                "JS 런타임: ✗ 없음 — YouTube 재생이 점차 불안정해질 수 있어요. `deno` 설치를 권장해요."
            } else {
                "JS runtime: ✗ none — YouTube playback may degrade over time; install `deno`."
            }
        );
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
        if verbose {
            print_linux_browser_verbose();
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

#[cfg(target_os = "linux")]
fn print_linux_browser_verbose() {
    println!("  browser diagnostics:");
    print_path_probe("xdg-open");
    print_path_probe("xdg-settings");
    print_path_probe("xdg-mime");
    print_path_probe("gio");
    println!(
        "    xdg-settings default-web-browser: {}",
        command_stdout("xdg-settings", &["get", "default-web-browser"])
    );
    println!(
        "    xdg-mime http handler: {}",
        command_stdout("xdg-mime", &["query", "default", "x-scheme-handler/http"])
    );
    println!(
        "    xdg-mime https handler: {}",
        command_stdout("xdg-mime", &["query", "default", "x-scheme-handler/https"])
    );
    println!(
        "    gio http handler: {}",
        command_stdout("gio", &["mime", "x-scheme-handler/http"])
    );
    println!(
        "    gio https handler: {}",
        command_stdout("gio", &["mime", "x-scheme-handler/https"])
    );
    println!("    environment:");
    for key in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
        "XDG_SESSION_TYPE",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_DATA_DIRS",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "BROWSER",
        "SNAP",
        "SNAP_NAME",
        "FLATPAK_ID",
        "WSL_DISTRO_NAME",
        "WSL_INTEROP",
    ] {
        println!("      {key}: {}", env_value(key));
    }
    println!(
        "    wsl detected: {}",
        if linux_wsl_detected() { "yes" } else { "no" }
    );
    println!(
        "    yututui DesktopOpen preserves: DISPLAY, WAYLAND_DISPLAY, XAUTHORITY, \
         XDG_RUNTIME_DIR, XDG_CONFIG_HOME, XDG_CACHE_HOME, XDG_DATA_HOME, \
         DBUS_SESSION_BUS_ADDRESS, XDG_DATA_DIRS, XDG_CURRENT_DESKTOP, \
         DESKTOP_SESSION, BROWSER"
    );
}

#[cfg(target_os = "linux")]
fn print_path_probe(bin: &str) {
    match deps::resolve_on_path(bin) {
        Some(path) => println!("    {bin}: {}", path.display()),
        None => println!("    {bin}: missing"),
    }
}

#[cfg(target_os = "linux")]
fn command_stdout(program: &str, args: &[&str]) -> String {
    if !deps::on_path(program) {
        return "missing".to_owned();
    }
    let mut cmd = crate::util::process::std_command(
        program,
        crate::util::process::ProcessProfile::DesktopOpen,
    );
    cmd.args(args);
    match crate::util::process::std_output_limited(
        cmd,
        crate::util::process::ProcessProfile::DesktopOpen,
        Duration::from_secs(2),
        4096,
    ) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if text.is_empty() {
                "(empty)".to_owned()
            } else {
                text
            }
        }
        Ok(out) => format!("failed ({})", out.status),
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(target_os = "linux")]
fn env_value(key: &str) -> String {
    std::env::var(key)
        .map(|v| {
            if v.is_empty() {
                "(empty)".to_owned()
            } else {
                v
            }
        })
        .unwrap_or_else(|_| "(unset)".to_owned())
}

#[cfg(target_os = "linux")]
fn linux_wsl_detected() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

/// The "Managed yt-dlp" section: whether the app-managed copy is enabled/installed,
/// its channel, and how fresh the last update check is.
fn print_managed_ytdlp(cfg: &config::Config, kr: bool) {
    use crate::tools::ytdlp;

    println!(
        "{}",
        if kr {
            "관리형 yt-dlp"
        } else {
            "Managed yt-dlp"
        }
    );
    if !cfg.tools.managed_enabled() {
        println!(
            "  - {}",
            if kr {
                "꺼짐 (tools.ytdlp_managed = false)"
            } else {
                "disabled (tools.ytdlp_managed = false)"
            }
        );
        println!();
        return;
    }
    if ytdlp::asset_name().is_none() {
        println!(
            "  - {}",
            if kr {
                "이 플랫폼용 공식 스탠드얼론 빌드가 없어 시스템 yt-dlp를 사용합니다"
            } else {
                "no official standalone build for this platform; the system yt-dlp is used"
            }
        );
        println!();
        return;
    }

    let state = ytdlp::load_state();
    let channel = state.channel.unwrap_or_else(|| cfg.tools.channel());
    match ytdlp::installed_managed_path() {
        Some(path) => println!(
            "  ✓ {} {} · {}",
            channel.label(),
            state.version.as_deref().unwrap_or("?"),
            path.display()
        ),
        None => println!(
            "  - {}",
            if kr {
                "설치되지 않음 — `ytt tools update`로 받거나, 앱 실행 시 자동으로 받습니다"
            } else {
                "not installed — fetch with `ytt tools update` (the app also fetches it automatically)"
            }
        ),
    }
    let checked = if kr { "마지막 확인" } else { "last check" };
    match state.last_check_unix {
        Some(at) => {
            let age_h = ytdlp::now_unix().saturating_sub(at) / 3600;
            println!("  - {checked}: {age_h}h");
        }
        None => println!("  - {checked}: {}", if kr { "없음" } else { "never" }),
    }
    println!();
}

fn print_ytdlp_verbose(cfg: &config::Config) {
    use crate::tools::ytdlp;

    println!("yt-dlp details");
    if let Some(error) = crate::tools::ytdlp_selection_error() {
        println!("  selection error: {error}");
    }
    match crate::tools::ytdlp_selection() {
        Some(sel) => {
            println!("  selected source: {}", sel.source.label());
            println!("  selected path: {}", sel.path.display());
            println!(
                "  selected version: {}",
                sel.version.as_deref().unwrap_or("?")
            );
            if let Some(actual) = inspect_sync(&sel.path) {
                println!("  selected actual version: {}", actual.version);
                println!("  selected sha256: {}", actual.sha256);
                println!(
                    "  selected file: mtime={} len={}",
                    actual.mtime_unix, actual.len
                );
            }
            if let Some(pin) = sel.pin_for_mpv() {
                println!("  mpv ytdl_path: {}", pin.display());
            }
        }
        None => println!("  selected: none"),
    }

    let state = ytdlp::load_state();
    println!("  managed enabled: {}", cfg.tools.managed_enabled());
    println!("  managed metadata channel: {:?}", state.channel);
    println!(
        "  managed metadata version: {}",
        state.version.as_deref().unwrap_or("?")
    );
    println!(
        "  managed metadata sha256: {}",
        state.sha256.as_deref().unwrap_or("?")
    );
    println!(
        "  managed metadata file: mtime={} len={}",
        state
            .installed_mtime_unix
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_owned()),
        state
            .installed_len
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_owned())
    );
    match ytdlp::installed_managed_path() {
        Some(path) => {
            println!("  managed path: {}", path.display());
            match inspect_sync(&path) {
                Some(actual) => {
                    println!("  managed actual version: {}", actual.version);
                    println!("  managed actual sha256: {}", actual.sha256);
                    println!(
                        "  managed actual file: mtime={} len={}",
                        actual.mtime_unix, actual.len
                    );
                }
                None => println!("  managed actual: probe failed"),
            }
        }
        None => println!("  managed path: not installed"),
    }

    let candidates = deps::resolve_all_on_path("yt-dlp");
    if candidates.is_empty() {
        println!("  PATH candidates: none");
    } else {
        println!("  PATH candidates:");
        for (idx, path) in candidates.iter().enumerate() {
            let version = inspect_sync(path)
                .map(|actual| actual.version)
                .unwrap_or_else(|| "?".to_owned());
            println!("    {}. {} · {}", idx + 1, version, path.display());
        }
    }
    println!();
}

fn inspect_sync(path: &Path) -> Option<crate::tools::ytdlp::BinaryInspection> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .and_then(|rt| rt.block_on(crate::tools::ytdlp::inspect_binary(path)).ok())
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

/// The per-user data directory, resolved through the shared [`crate::paths::data_dir`].
fn data_dir() -> Option<PathBuf> {
    crate::paths::data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::env::{with_var, with_vars};

    const TERMINAL_ENV: &[(&str, Option<&str>)] = &[
        ("KITTY_WINDOW_ID", None),
        ("WEZTERM_EXECUTABLE", None),
        ("KONSOLE_VERSION", None),
        ("WT_SESSION", None),
        ("TERM", None),
        ("TERM_PROGRAM", None),
    ];

    fn with_terminal_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let mut scoped = TERMINAL_ENV.to_vec();
        scoped.extend_from_slice(vars);
        with_vars(&scoped, f)
    }

    #[test]
    fn terminal_doctor_detects_common_protocol_hints_without_probing() {
        with_var("YTM_TUI_TEXT_SIZING", None, || {
            assert_eq!(
                terminal_image_protocol(Some("xterm-kitty"), None, false, None),
                "kitty"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), Some("WezTerm"), false, None),
                "iterm2_or_kitty_or_sixel"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), Some("iTerm.app"), false, None),
                "iterm2"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), None, true, None),
                "sixel_versioned"
            );
            assert_eq!(
                terminal_zoom_mode(Some("xterm-kitty"), None, false),
                "osc66_versioned"
            );
            assert_eq!(
                terminal_zoom_mode(Some("xterm-256color"), None, true),
                "decdhl_expected"
            );
        });
    }

    #[test]
    fn terminal_doctor_covers_protocol_keyboard_and_zoom_edges() {
        with_var("YTM_TUI_TEXT_SIZING", None, || {
            assert_eq!(
                terminal_image_protocol(Some("foot"), None, false, None),
                "sixel"
            );
            assert_eq!(
                terminal_image_protocol(Some("mintty"), None, false, None),
                "sixel"
            );
            assert_eq!(
                terminal_image_protocol(Some("konsole"), None, false, None),
                "halfblocks"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), None, false, Some("invalid")),
                "halfblocks"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), None, false, Some("260399")),
                "halfblocks"
            );
            assert_eq!(
                terminal_image_protocol(Some("konsole-256color"), None, false, Some("260400")),
                "sixel_versioned"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), None, false, Some("260401")),
                "sixel_versioned"
            );
            assert_eq!(
                terminal_image_protocol(Some("ghostty"), Some("ignored"), false, None),
                "kitty"
            );
            assert_eq!(
                terminal_image_protocol(Some("linux"), None, false, None),
                "halfblocks_or_retro"
            );
            assert_eq!(
                terminal_image_protocol(Some("dumb"), None, false, None),
                "unknown"
            );
            assert_eq!(
                terminal_zoom_mode(Some("plain"), Some("Ghostty"), false),
                "unknown_probe_required"
            );
            assert_eq!(
                terminal_zoom_mode(Some("plain"), Some("WezTerm"), false),
                "unknown_probe_required"
            );
            assert_eq!(terminal_zoom_mode(Some("plain"), None, false), "unknown");
            assert_eq!(
                terminal_keyboard_hint(Some("foot"), None, false),
                Some(true)
            );
            assert_eq!(
                terminal_keyboard_hint(Some("xterm"), Some("ghostty"), false),
                Some(true)
            );
            assert_eq!(
                terminal_keyboard_hint(Some("xterm"), None, true),
                Some(true)
            );
        });

        with_var("YTM_TUI_TEXT_SIZING", Some("false"), || {
            assert_eq!(terminal_zoom_mode(None, None, false), "none_forced");
        });
        with_var("YTM_TUI_TEXT_SIZING", Some("DHL"), || {
            assert_eq!(terminal_zoom_mode(None, None, false), "decdhl_forced");
        });
        with_var("YTM_TUI_TEXT_SIZING", Some("probe"), || {
            assert_eq!(terminal_zoom_mode(None, None, false), "probe_requested");
        });
    }

    #[test]
    fn terminal_doctor_reports_native_hint_timeout_and_override_guidance() {
        with_terminal_env(&[], || {
            assert!(!terminal_native_image_hint(
                Some("xterm-256color"),
                Some("plain-terminal"),
                false
            ));
            assert_eq!(terminal_image_probe_timeout_ms(false), 250);
            assert!(
                terminal_image_override_suggestions(
                    Some("xterm-256color"),
                    Some("plain-terminal"),
                    false
                )
                .is_empty()
            );
        });

        with_terminal_env(&[("TERM", Some("foot"))], || {
            assert!(terminal_native_image_hint(Some("foot"), None, false));
            assert_eq!(terminal_image_probe_timeout_ms(true), 700);
            assert_eq!(
                terminal_image_override_suggestions(Some("foot"), None, false),
                vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"]
            );
        });

        with_terminal_env(&[("TERM", Some("ghostty"))], || {
            assert!(terminal_native_image_hint(Some("ghostty"), None, false));
            assert_eq!(
                terminal_image_override_suggestions(Some("ghostty"), None, false),
                vec!["YTM_TUI_IMAGE_PROTOCOL=kitty"]
            );
        });

        with_terminal_env(&[("KONSOLE_VERSION", Some("260400"))], || {
            assert!(terminal_native_image_hint(None, None, false));
            assert_eq!(
                terminal_image_override_suggestions(None, None, false),
                vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"]
            );
        });

        with_terminal_env(&[("WEZTERM_EXECUTABLE", Some("wezterm"))], || {
            assert!(terminal_native_image_hint(None, None, false));
            assert_eq!(
                terminal_image_override_suggestions(None, None, false),
                vec![
                    "YTM_TUI_IMAGE_PROTOCOL=iterm2",
                    "YTM_TUI_IMAGE_PROTOCOL=kitty",
                    "YTM_TUI_IMAGE_PROTOCOL=sixel"
                ]
            );
        });
    }

    #[test]
    fn terminal_doctor_validates_image_protocol_overrides() {
        assert!(image_protocol_override_supported("halfblocks"));
        assert!(image_protocol_override_supported("  SIXEL  "));
        assert!(image_protocol_override_supported("kitty"));
        assert!(image_protocol_override_supported("iterm2"));
        assert!(!image_protocol_override_supported("bad"));
    }

    #[test]
    fn terminal_doctor_marks_unknown_keyboard_support_as_unknown() {
        assert_eq!(terminal_keyboard_hint(Some("dumb"), None, false), None);
        assert_eq!(
            terminal_keyboard_hint(Some("xterm-kitty"), None, false),
            Some(true)
        );
    }

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

    #[test]
    fn a_missing_dir_below_a_file_is_not_writable() {
        let path =
            std::env::temp_dir().join(format!("ytt-doctor-file-anchor-{}", std::process::id()));
        std::fs::write(&path, b"file").expect("write temp file");

        assert!(!dir_is_writable(&path.join("child")));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn privacy_path_redacts_home_prefix() {
        let Some(base) = directories::BaseDirs::new() else {
            return;
        };
        let path = base.home_dir().join(".config/yututui/config.json");
        let display = privacy_path(&path);
        let home = base.home_dir().to_string_lossy();
        assert!(display.starts_with("~/"), "{display}");
        assert!(!display.contains(home.as_ref()));
    }

    #[test]
    fn run_with_args_handles_help_terminal_json_and_bad_usage_without_full_doctor() {
        assert_eq!(run_with_args(&["--help".to_owned()]), 0);
        assert_eq!(
            run_with_args(&["privacy".to_owned(), "--help".to_owned()]),
            0
        );
        assert_eq!(
            run_with_args(&["terminal".to_owned(), "--help".to_owned()]),
            0
        );
        assert_eq!(
            run_with_args(&["terminal".to_owned(), "--json".to_owned()]),
            0
        );
        assert_eq!(run_with_args(&["--bogus".to_owned()]), 2);
    }
}
