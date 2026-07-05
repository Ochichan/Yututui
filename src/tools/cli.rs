//! `ytt tools` — manage the app-managed yt-dlp from the command line.
//!
//! `status` prints which yt-dlp/mpv the app would use (same resolution as startup);
//! `update` forces a check-and-install against the configured channel; `use`/`unpin`
//! make the yt-dlp choice explicit when a user needs to recover from a bad upstream
//! release. All run in the synchronous main path before any terminal setup, like
//! `ytt doctor`.

use std::path::{Path, PathBuf};

use crate::{config, deps, i18n, tools};

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None => status(false),
        Some("status") => match args.get(1).map(String::as_str) {
            None => status(false),
            Some("--why") => status(true),
            Some("--help" | "-h") => {
                help();
                0
            }
            Some(other) => {
                eprintln!("ytt tools status: unknown option `{other}`");
                help();
                2
            }
        },
        Some("use") => use_ytdlp(&args[1..]),
        Some("unpin") => unpin_ytdlp(),
        Some("update") => update(),
        Some("--help" | "-h" | "help") => {
            help();
            0
        }
        Some(other) => {
            eprintln!("ytt tools: unknown command `{other}`");
            help();
            2
        }
    }
}

fn help() {
    println!("Usage: ytt tools <command>");
    println!();
    println!("Commands:");
    println!("  status   Show which yt-dlp/mpv the app uses (managed, system, or override)");
    println!("           Use `ytt tools status --why` to show candidate selection reasons");
    println!("  use      Pin yt-dlp to `system`, `managed`, or an explicit executable path");
    println!("  unpin    Return yt-dlp selection to the normal managed/system policy");
    println!("  update   Check the release channel now and install a newer yt-dlp if available");
}

/// A current-thread runtime for the one-shot commands (precedent: the auth/transfer
/// subcommands — never the multi-thread TUI runtime).
fn block_on<F: std::future::Future>(fut: F) -> Option<F::Output> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(fut))
}

fn status(why: bool) -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    let Some(()) = block_on(tools::init(&cfg.tools)) else {
        eprintln!("ytt tools: failed to build async runtime");
        return 1;
    };

    match tools::ytdlp_selection() {
        Some(sel) => println!(
            "yt-dlp: {} {} · {}",
            selection_label(&cfg, &sel),
            sel.version.as_deref().unwrap_or("?"),
            sel.path.display()
        ),
        None => println!(
            "yt-dlp: {}",
            if kr {
                "없음 — `ytt tools update`로 받으세요"
            } else {
                "none found — fetch one with `ytt tools update`"
            }
        ),
    }

    let state = tools::ytdlp::load_state();
    let channel = state.channel.unwrap_or_else(|| cfg.tools.channel());
    if !cfg.tools.managed_enabled() {
        println!(
            "managed: {}",
            if kr {
                "꺼짐 (tools.ytdlp_managed = false)"
            } else {
                "disabled (tools.ytdlp_managed = false)"
            }
        );
    } else if tools::ytdlp::asset_name().is_none() {
        println!(
            "managed: {}",
            if kr {
                "이 플랫폼은 미지원 (시스템 yt-dlp 사용)"
            } else {
                "unsupported on this platform (system yt-dlp is used)"
            }
        );
    } else {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => println!(
                "managed: {} {} · {}",
                channel.label(),
                state.version.as_deref().unwrap_or("?"),
                path.display()
            ),
            None => println!(
                "managed: {} — {}",
                channel.label(),
                if kr {
                    "설치되지 않음"
                } else {
                    "not installed"
                }
            ),
        }
        match state.last_check_unix {
            Some(at) => {
                let age_h = tools::ytdlp::now_unix().saturating_sub(at) / 3600;
                println!(
                    "{}: {age_h}h",
                    if kr { "마지막 확인" } else { "last check" }
                );
            }
            None => println!(
                "{}: {}",
                if kr { "마지막 확인" } else { "last check" },
                if kr { "없음" } else { "never" }
            ),
        }
    }

    println!("mpv: {}", cfg.tools.mpv_program());
    if why {
        print_status_why(&cfg, kr);
    }
    match tools::ytdlp_selection() {
        Some(_) => 0,
        None => 1,
    }
}

fn selection_label(cfg: &config::Config, sel: &tools::YtdlpSelection) -> &'static str {
    if sel.source == tools::YtdlpSource::Override
        && active_env_ytdlp_override().is_none()
        && tools::ytdlp::installed_managed_path()
            .as_ref()
            .is_some_and(|managed| same_path(managed, &sel.path))
        && cfg
            .tools
            .ytdlp_path
            .as_ref()
            .is_some_and(|p| same_path(p, &sel.path))
    {
        return "managed-pinned";
    }
    sel.source.label()
}

fn same_path(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    a == b
}

fn print_status_why(cfg: &config::Config, kr: bool) {
    println!();
    println!(
        "{}",
        if kr {
            "선택 이유"
        } else {
            "Selection reasons"
        }
    );
    println!(
        "  - {}",
        if kr {
            "정책: override > enabled managed/system 최신 버전 비교; 같은 버전이면 managed 우선"
        } else {
            "policy: override > newest enabled managed/system; equal versions prefer managed"
        }
    );
    if cfg!(target_os = "macos") {
        println!(
            "  - {}",
            if kr {
                "macOS 예외: 실행 가능한 system yt-dlp가 있으면 managed보다 우선 (스탠드얼론 실행 지연 회피)"
            } else {
                "macOS exception: a usable system yt-dlp wins over managed to avoid standalone exec latency"
            }
        );
    }

    match cfg.tools.ytdlp_override() {
        Some(path) => {
            let version = probe_ytdlp_path(&path).unwrap_or_else(|| "?".to_owned());
            println!(
                "  - override: {} {} · {}",
                if kr { "활성" } else { "active" },
                version,
                path.display()
            );
            print_js_runtime_why(kr);
            return;
        }
        None => println!(
            "  - override: {}",
            if kr { "설정되지 않음" } else { "not set" }
        ),
    }

    if !cfg.tools.managed_enabled() {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => println!(
                "  - managed candidate: {} · {}",
                if kr {
                    "무시됨 (tools.ytdlp_managed = false)"
                } else {
                    "ignored (tools.ytdlp_managed = false)"
                },
                path.display()
            ),
            None => println!(
                "  - managed candidate: {}",
                if kr {
                    "무시됨 (tools.ytdlp_managed = false)"
                } else {
                    "ignored (tools.ytdlp_managed = false)"
                }
            ),
        }
    } else if tools::ytdlp::asset_name().is_none() {
        println!(
            "  - managed candidate: {}",
            if kr {
                "이 플랫폼은 공식 managed 빌드 없음"
            } else {
                "no official managed build for this platform"
            }
        );
    } else {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => {
                let version = probe_ytdlp_path(&path).unwrap_or_else(|| "?".to_owned());
                println!("  - managed candidate: {version} · {}", path.display());
            }
            None => println!(
                "  - managed candidate: {}",
                if kr {
                    "설치되지 않음"
                } else {
                    "not installed"
                }
            ),
        }
    }

    match deps::resolve_on_path("yt-dlp") {
        Some(path) => {
            let version = probe_ytdlp_path(&path).unwrap_or_else(|| "?".to_owned());
            println!("  - system candidate: {version} · {}", path.display());
        }
        None => println!(
            "  - system candidate: {}",
            if kr {
                "PATH에서 찾지 못함"
            } else {
                "not found on PATH"
            }
        ),
    }

    print_js_runtime_why(kr);
}

fn probe_ytdlp_path(path: &Path) -> Option<String> {
    block_on(tools::ytdlp::cached_probe(path)).flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UseTarget {
    System,
    Managed,
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PinKind {
    System,
    Managed,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPin {
    kind: PinKind,
    path: PathBuf,
    version: String,
}

fn use_ytdlp(args: &[String]) -> i32 {
    if args.len() != 1 {
        eprintln!("ytt tools use: expected exactly one target");
        eprintln!("usage: ytt tools use <system|managed|path>");
        return 2;
    }

    let mut cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    let target = match parse_use_target(&args[0]) {
        Ok(target) => target,
        Err(msg) => {
            eprintln!("ytt tools use: {msg}");
            eprintln!("usage: ytt tools use <system|managed|path>");
            return 2;
        }
    };

    let pin = match resolve_pin_target(&target, kr) {
        Ok(pin) => pin,
        Err(msg) => {
            eprintln!("ytt tools use: {msg}");
            return 1;
        }
    };

    apply_pin(&mut cfg, &pin);
    if let Err(e) = cfg.save() {
        eprintln!("ytt tools use: failed to save config: {e}");
        return 1;
    }

    println!(
        "{} {} · {}",
        match pin.kind {
            PinKind::System => {
                if kr {
                    "yt-dlp를 system에 고정했습니다:"
                } else {
                    "yt-dlp pinned to system:"
                }
            }
            PinKind::Managed => {
                if kr {
                    "yt-dlp를 managed에 고정했습니다:"
                } else {
                    "yt-dlp pinned to managed:"
                }
            }
            PinKind::Path => {
                if kr {
                    "yt-dlp를 지정 경로에 고정했습니다:"
                } else {
                    "yt-dlp pinned to path:"
                }
            }
        },
        pin.version,
        pin.path.display()
    );
    warn_env_override(kr);
    0
}

fn unpin_ytdlp() -> i32 {
    let mut cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    cfg.tools.ytdlp_path = None;
    cfg.tools.ytdlp_managed = None;
    if let Err(e) = cfg.save() {
        eprintln!("ytt tools unpin: failed to save config: {e}");
        return 1;
    }

    println!(
        "{}",
        if kr {
            "yt-dlp 고정을 해제했습니다. 기본 managed/system 선택 정책을 사용합니다."
        } else {
            "yt-dlp unpinned. The normal managed/system selection policy is active."
        }
    );
    warn_env_override(kr);
    0
}

fn parse_use_target(raw: &str) -> Result<UseTarget, &'static str> {
    let target = raw.trim();
    if target.is_empty() {
        return Err("target is empty");
    }

    match target.to_ascii_lowercase().as_str() {
        "system" => Ok(UseTarget::System),
        "managed" => Ok(UseTarget::Managed),
        _ if looks_like_path(target) => Ok(UseTarget::Path(PathBuf::from(target))),
        _ => Err("target must be `system`, `managed`, or an explicit path"),
    }
}

fn looks_like_path(raw: &str) -> bool {
    let path = Path::new(raw);
    path.is_absolute() || path.components().count() > 1 || raw.contains('\\')
}

fn resolve_pin_target(target: &UseTarget, kr: bool) -> Result<ResolvedPin, String> {
    match target {
        UseTarget::System => {
            let path = deps::resolve_on_path("yt-dlp").ok_or_else(|| {
                if kr {
                    "PATH에서 system yt-dlp를 찾지 못했습니다.".to_owned()
                } else {
                    "system yt-dlp was not found on PATH.".to_owned()
                }
            })?;
            let version = probe_ytdlp_path(&path).ok_or_else(|| {
                if kr {
                    format!(
                        "system yt-dlp 버전을 확인할 수 없습니다: {}",
                        path.display()
                    )
                } else {
                    format!("could not read system yt-dlp version: {}", path.display())
                }
            })?;
            Ok(ResolvedPin {
                kind: PinKind::System,
                path,
                version,
            })
        }
        UseTarget::Managed => {
            let path = tools::ytdlp::installed_managed_path().ok_or_else(|| {
                if kr {
                    "managed yt-dlp가 설치되어 있지 않습니다. 먼저 `ytt tools update`를 실행하세요."
                        .to_owned()
                } else {
                    "managed yt-dlp is not installed. Run `ytt tools update` first.".to_owned()
                }
            })?;
            let version = probe_ytdlp_path(&path).ok_or_else(|| {
                if kr {
                    format!(
                        "managed yt-dlp 버전을 확인할 수 없습니다: {}",
                        path.display()
                    )
                } else {
                    format!("could not read managed yt-dlp version: {}", path.display())
                }
            })?;
            Ok(ResolvedPin {
                kind: PinKind::Managed,
                path,
                version,
            })
        }
        UseTarget::Path(raw_path) => {
            let path = std::fs::canonicalize(raw_path).map_err(|e| {
                if kr {
                    format!(
                        "지정한 yt-dlp 경로를 열 수 없습니다: {} ({e})",
                        raw_path.display()
                    )
                } else {
                    format!("could not open yt-dlp path: {} ({e})", raw_path.display())
                }
            })?;
            let path_str = path.to_string_lossy();
            if !deps::on_path(path_str.as_ref()) {
                return Err(if kr {
                    format!("지정한 yt-dlp가 실행 파일이 아닙니다: {}", path.display())
                } else {
                    format!("yt-dlp path is not executable: {}", path.display())
                });
            }
            let version = probe_ytdlp_path(&path).ok_or_else(|| {
                if kr {
                    format!(
                        "지정한 yt-dlp 버전을 확인할 수 없습니다: {}",
                        path.display()
                    )
                } else {
                    format!("could not read yt-dlp version: {}", path.display())
                }
            })?;
            Ok(ResolvedPin {
                kind: PinKind::Path,
                path,
                version,
            })
        }
    }
}

fn apply_pin(cfg: &mut config::Config, pin: &ResolvedPin) {
    match pin.kind {
        PinKind::System => {
            cfg.tools.ytdlp_path = None;
            cfg.tools.ytdlp_managed = Some(false);
        }
        PinKind::Managed => {
            cfg.tools.ytdlp_path = Some(pin.path.clone());
            cfg.tools.ytdlp_managed = Some(true);
        }
        PinKind::Path => {
            cfg.tools.ytdlp_path = Some(pin.path.clone());
        }
    }
}

fn warn_env_override(kr: bool) {
    let Some(value) = active_env_ytdlp_override() else {
        return;
    };

    eprintln!(
        "{}",
        if kr {
            format!(
                "주의: 현재 프로세스에는 YTM_YTDLP={value} 가 설정되어 있어 config보다 우선합니다."
            )
        } else {
            format!("note: YTM_YTDLP={value} is set and still overrides config.")
        }
    );
}

fn active_env_ytdlp_override() -> Option<String> {
    std::env::var("YTM_YTDLP")
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn print_js_runtime_why(kr: bool) {
    let probes = tools::js_runtime_diagnostics();
    if let Some(probe) = probes.iter().find(|probe| probe.supported) {
        let version = probe
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let mode = if probe.runtime.flag_value().is_none() {
            if kr { "자동 사용" } else { "auto-used" }
        } else if kr {
            "--js-runtimes 로 연결"
        } else {
            "wired via --js-runtimes"
        };
        println!(
            "  - JS runtime: supported {}{} ({mode})",
            probe.runtime.label(),
            version
        );
    } else if let Some(probe) = probes.first() {
        let version = probe
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        println!(
            "  - JS runtime: unsupported {}{} ({})",
            probe.runtime.label(),
            version,
            probe.reason.unwrap_or("unsupported version")
        );
    } else {
        println!("  - JS runtime: {}", if kr { "없음" } else { "none found" });
    }
}

fn update() -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    let outcome = block_on(async {
        tools::init(&cfg.tools).await;
        tools::ytdlp::check_and_update(&cfg.tools, &|event| match event {
            tools::ToolsEvent::Progress {
                channel,
                percent: Some(p),
            } => println!("  … {p:>3}% ({})", channel.label()),
            tools::ToolsEvent::Progress { channel, .. } => println!(
                "{} ({})…",
                if kr {
                    "yt-dlp 다운로드 중"
                } else {
                    "downloading yt-dlp"
                },
                channel.label()
            ),
            // Installed/Failed become the outcome lines below.
            tools::ToolsEvent::Installed { .. } | tools::ToolsEvent::Failed { .. } => {}
        })
        .await
    });
    let Some(outcome) = outcome else {
        eprintln!("ytt tools: failed to build async runtime");
        return 1;
    };

    match outcome {
        tools::ytdlp::UpdateOutcome::Installed { version } => {
            println!(
                "{}",
                if kr {
                    format!("yt-dlp {version} 설치 완료.")
                } else {
                    format!("yt-dlp {version} installed.")
                }
            );
            0
        }
        tools::ytdlp::UpdateOutcome::AlreadyCurrent => {
            let state = tools::ytdlp::load_state();
            println!(
                "{}",
                if kr {
                    format!(
                        "이미 최신입니다 ({}).",
                        state.version.as_deref().unwrap_or("?")
                    )
                } else {
                    format!(
                        "Already up to date ({}).",
                        state.version.as_deref().unwrap_or("?")
                    )
                }
            );
            0
        }
        tools::ytdlp::UpdateOutcome::Unavailable(e) => {
            eprintln!("ytt tools update: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_use_target_accepts_system_managed_and_paths() {
        assert_eq!(parse_use_target("system"), Ok(UseTarget::System));
        assert_eq!(parse_use_target("MANAGED"), Ok(UseTarget::Managed));

        match parse_use_target("./yt-dlp") {
            Ok(UseTarget::Path(path)) => assert_eq!(path, PathBuf::from("./yt-dlp")),
            other => panic!("expected relative path, got {other:?}"),
        }

        match parse_use_target(r"C:\tools\yt-dlp.exe") {
            Ok(UseTarget::Path(path)) => assert_eq!(path, PathBuf::from(r"C:\tools\yt-dlp.exe")),
            other => panic!("expected windows path, got {other:?}"),
        }

        assert!(parse_use_target("nightly").is_err());
    }

    #[test]
    fn system_pin_clears_path_and_excludes_managed_candidate() {
        let mut cfg = config::Config::default();
        cfg.tools.ytdlp_path = Some(PathBuf::from("/old/yt-dlp"));
        cfg.tools.ytdlp_managed = Some(true);

        apply_pin(
            &mut cfg,
            &ResolvedPin {
                kind: PinKind::System,
                path: PathBuf::from("/usr/bin/yt-dlp"),
                version: "2026.06.09".to_owned(),
            },
        );

        assert_eq!(cfg.tools.ytdlp_path, None);
        assert_eq!(cfg.tools.ytdlp_managed, Some(false));
    }

    #[test]
    fn managed_pin_uses_installed_path_as_an_explicit_override() {
        let mut cfg = config::Config::default();
        cfg.tools.ytdlp_managed = Some(false);

        apply_pin(
            &mut cfg,
            &ResolvedPin {
                kind: PinKind::Managed,
                path: PathBuf::from("/data/tools/yt-dlp"),
                version: "2026.07.04.221833".to_owned(),
            },
        );

        assert_eq!(
            cfg.tools.ytdlp_path,
            Some(PathBuf::from("/data/tools/yt-dlp"))
        );
        assert_eq!(cfg.tools.ytdlp_managed, Some(true));
    }

    #[test]
    fn path_pin_keeps_managed_policy_unchanged() {
        let mut cfg = config::Config::default();
        cfg.tools.ytdlp_managed = Some(false);

        apply_pin(
            &mut cfg,
            &ResolvedPin {
                kind: PinKind::Path,
                path: PathBuf::from("/custom/yt-dlp"),
                version: "2026.06.09".to_owned(),
            },
        );

        assert_eq!(cfg.tools.ytdlp_path, Some(PathBuf::from("/custom/yt-dlp")));
        assert_eq!(cfg.tools.ytdlp_managed, Some(false));
    }
}
