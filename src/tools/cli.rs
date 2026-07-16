//! `ytt tools` — manage the app-managed yt-dlp from the command line.
//!
//! `status` prints which yt-dlp/mpv the app would use (same resolution as startup);
//! `update` forces a check-and-install against the configured channel; `use`/`unpin`
//! make the yt-dlp choice explicit when a user needs to recover from a bad upstream
//! release. All run in the synchronous main path before any terminal setup, like
//! `ytt doctor`.

use std::path::{Path, PathBuf};

use crate::{config, deps, i18n, session, tools};

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
        Some("reset") => reset(&args[1..]),
        Some("diagnose") => diagnose(),
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
    println!("  reset    Clear transient playback/tool state (`ytt tools reset --playback`)");
    println!("  diagnose Write a yt-dlp/mpv diagnostic bundle for bug reports");
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
    let lang = i18n::current();

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
        None if tools::ytdlp_selection_error().is_some() => println!(
            "yt-dlp: {}",
            tools::ytdlp_selection_error().unwrap_or_default()
        ),
        None => println!(
            "yt-dlp: {}",
            match lang {
                i18n::Language::Korean => "없음 — `ytt tools update`로 받으세요",
                i18n::Language::Japanese => "なし — `ytt tools update`で取得してください",
                _ => "none found — fetch one with `ytt tools update`",
            }
        ),
    }

    let state = tools::ytdlp::load_state();
    let channel = state.channel.unwrap_or_else(|| cfg.tools.channel());
    if !cfg.tools.managed_enabled() {
        println!(
            "managed: {}",
            match lang {
                i18n::Language::Korean => "꺼짐 (tools.ytdlp_managed = false)",
                i18n::Language::Japanese => "無効 (tools.ytdlp_managed = false)",
                _ => "disabled (tools.ytdlp_managed = false)",
            }
        );
    } else if tools::ytdlp::asset_name().is_none() {
        println!(
            "managed: {}",
            match lang {
                i18n::Language::Korean => "이 플랫폼은 미지원 (시스템 yt-dlp 사용)",
                i18n::Language::Japanese => "このプラットフォームは未対応 (システムのyt-dlpを使用)",
                _ => "unsupported on this platform (system yt-dlp is used)",
            }
        );
    } else {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => {
                let actual = probe_ytdlp_path(&path).unwrap_or_else(|| "?".to_owned());
                println!(
                    "managed: {} metadata={} actual={} · {}",
                    channel.label(),
                    state.version.as_deref().unwrap_or("?"),
                    actual,
                    path.display()
                );
            }
            None => println!(
                "managed: {} — {}",
                channel.label(),
                match lang {
                    i18n::Language::Korean => "설치되지 않음",
                    i18n::Language::Japanese => "未インストール",
                    _ => "not installed",
                }
            ),
        }
        let last_check = match lang {
            i18n::Language::Korean => "마지막 확인",
            i18n::Language::Japanese => "最終確認",
            _ => "last check",
        };
        match state.last_check_unix {
            Some(at) => {
                let age_h = tools::ytdlp::now_unix().saturating_sub(at) / 3600;
                println!("{last_check}: {age_h}h");
            }
            None => println!(
                "{last_check}: {}",
                match lang {
                    i18n::Language::Korean => "없음",
                    i18n::Language::Japanese => "なし",
                    _ => "never",
                }
            ),
        }
    }

    println!("mpv: {}", cfg.tools.mpv_program());
    if why {
        print_status_why(&cfg, lang);
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

fn print_status_why(cfg: &config::Config, lang: i18n::Language) {
    println!();
    println!(
        "{}",
        match lang {
            i18n::Language::Korean => "선택 이유",
            i18n::Language::Japanese => "選択理由",
            _ => "Selection reasons",
        }
    );
    println!(
        "  - {}",
        match lang {
            i18n::Language::Korean =>
                "정책: override > enabled managed/system 최신 버전 비교; 같은 버전이면 managed 우선",
            i18n::Language::Japanese =>
                "ポリシー: override > 有効な managed/system の新しい方; 同一バージョンなら managed 優先",
            _ => "policy: override > newest enabled managed/system; equal versions prefer managed",
        }
    );
    if cfg!(target_os = "macos") {
        println!(
            "  - {}",
            match lang {
                i18n::Language::Korean =>
                    "macOS 예외: 실행 가능한 system yt-dlp가 있으면 managed보다 우선 (스탠드얼론 실행 지연 회피)",
                i18n::Language::Japanese =>
                    "macOS 例外: 実行可能な system yt-dlp があれば managed より優先 (スタンドアロン実行の遅延回避)",
                _ =>
                    "macOS exception: a usable system yt-dlp wins over managed to avoid standalone exec latency",
            }
        );
    }

    let ignored_managed = match lang {
        i18n::Language::Korean => "무시됨 (tools.ytdlp_managed = false)",
        i18n::Language::Japanese => "無視 (tools.ytdlp_managed = false)",
        _ => "ignored (tools.ytdlp_managed = false)",
    };
    match cfg.tools.ytdlp_override() {
        Some(path) => {
            let version = probe_ytdlp_path(&path).unwrap_or_else(|| "?".to_owned());
            println!(
                "  - override: {} {} · {}",
                match lang {
                    i18n::Language::Korean => "활성",
                    i18n::Language::Japanese => "有効",
                    _ => "active",
                },
                version,
                path.display()
            );
            print_js_runtime_why(lang);
            return;
        }
        None => println!(
            "  - override: {}",
            match lang {
                i18n::Language::Korean => "설정되지 않음",
                i18n::Language::Japanese => "未設定",
                _ => "not set",
            }
        ),
    }

    if !cfg.tools.managed_enabled() {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => println!(
                "  - managed candidate: {} · {}",
                ignored_managed,
                path.display()
            ),
            None => println!("  - managed candidate: {ignored_managed}"),
        }
    } else if tools::ytdlp::asset_name().is_none() {
        println!(
            "  - managed candidate: {}",
            match lang {
                i18n::Language::Korean => "이 플랫폼은 공식 managed 빌드 없음",
                i18n::Language::Japanese => "このプラットフォーム向けの公式 managed ビルドなし",
                _ => "no official managed build for this platform",
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
                match lang {
                    i18n::Language::Korean => "설치되지 않음",
                    i18n::Language::Japanese => "未インストール",
                    _ => "not installed",
                }
            ),
        }
    }

    let system_candidates = deps::resolve_all_on_path("yt-dlp");
    if system_candidates.is_empty() {
        println!(
            "  - system candidates: {}",
            match lang {
                i18n::Language::Korean => "PATH에서 찾지 못함",
                i18n::Language::Japanese => "PATHに見つかりません",
                _ => "not found on PATH",
            }
        );
    } else {
        println!("  - system candidates:");
        for (idx, path) in system_candidates.iter().enumerate() {
            let version = probe_ytdlp_path(path).unwrap_or_else(|| "?".to_owned());
            println!("    {}. {version} · {}", idx + 1, path.display());
        }
    }

    print_js_runtime_why(lang);
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
    let lang = i18n::current();

    let target = match parse_use_target(&args[0]) {
        Ok(target) => target,
        Err(msg) => {
            eprintln!("ytt tools use: {msg}");
            eprintln!("usage: ytt tools use <system|managed|path>");
            return 2;
        }
    };

    let pin = match resolve_pin_target(&target, lang) {
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
            PinKind::System => match lang {
                i18n::Language::Korean => "yt-dlp를 system에 고정했습니다:",
                i18n::Language::Japanese => "yt-dlpをsystemに固定しました:",
                _ => "yt-dlp pinned to system:",
            },
            PinKind::Managed => match lang {
                i18n::Language::Korean => "yt-dlp를 managed에 고정했습니다:",
                i18n::Language::Japanese => "yt-dlpをmanagedに固定しました:",
                _ => "yt-dlp pinned to managed:",
            },
            PinKind::Path => match lang {
                i18n::Language::Korean => "yt-dlp를 지정 경로에 고정했습니다:",
                i18n::Language::Japanese => "yt-dlpを指定パスに固定しました:",
                _ => "yt-dlp pinned to path:",
            },
        },
        pin.version,
        pin.path.display()
    );
    warn_env_override(lang);
    0
}

fn unpin_ytdlp() -> i32 {
    let mut cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();

    cfg.tools.ytdlp_path = None;
    cfg.tools.ytdlp_managed = None;
    if let Err(e) = cfg.save() {
        eprintln!("ytt tools unpin: failed to save config: {e}");
        return 1;
    }

    println!(
        "{}",
        match lang {
            i18n::Language::Korean =>
                "yt-dlp 고정을 해제했습니다. 기본 managed/system 선택 정책을 사용합니다.",
            i18n::Language::Japanese =>
                "yt-dlpの固定を解除しました。通常のmanaged/system選択ポリシーを使用します。",
            _ => "yt-dlp unpinned. The normal managed/system selection policy is active.",
        }
    );
    warn_env_override(lang);
    0
}

fn reset(args: &[String]) -> i32 {
    if args.len() != 1 || args[0] != "--playback" {
        eprintln!("usage: ytt tools reset --playback");
        return 2;
    }
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();

    let mut ok = true;
    match session::SessionCache::clear() {
        Ok(true) => println!(
            "{}",
            match lang {
                i18n::Language::Korean => "session cache: 삭제됨",
                i18n::Language::Japanese => "session cache: 削除済み",
                _ => "session cache: cleared",
            }
        ),
        Ok(false) => println!(
            "{}",
            match lang {
                i18n::Language::Korean => "session cache: 없음",
                i18n::Language::Japanese => "session cache: なし",
                _ => "session cache: not present",
            }
        ),
        Err(e) => {
            ok = false;
            eprintln!("session cache: {e}");
        }
    }

    tools::ytdlp::clear_probe_cache();
    println!(
        "{}",
        match lang {
            i18n::Language::Korean => "yt-dlp probe cache: 삭제됨",
            i18n::Language::Japanese => "yt-dlp probe cache: 削除済み",
            _ => "yt-dlp probe cache: cleared",
        }
    );

    match tools::ytdlp::remove_update_lock_if_free() {
        Ok(true) => println!(
            "{}",
            match lang {
                i18n::Language::Korean => "yt-dlp update lock: stale lock 삭제됨",
                i18n::Language::Japanese => "yt-dlp update lock: stale lock 削除済み",
                _ => "yt-dlp update lock: stale lock removed",
            }
        ),
        Ok(false) => println!(
            "{}",
            match lang {
                i18n::Language::Korean => "yt-dlp update lock: 없음 또는 사용 중",
                i18n::Language::Japanese => "yt-dlp update lock: なしまたは使用中",
                _ => "yt-dlp update lock: absent or busy",
            }
        ),
        Err(e) => {
            ok = false;
            eprintln!("yt-dlp update lock: {e}");
        }
    }

    if block_on(tools::init(&cfg.tools)).is_none() {
        ok = false;
        eprintln!("ytt tools reset: failed to refresh tool selection");
    }
    if let Some(err) = tools::ytdlp_selection_error() {
        ok = false;
        eprintln!("yt-dlp selection: {err}");
    }

    if ok { 0 } else { 1 }
}

fn diagnose() -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();
    if block_on(tools::init(&cfg.tools)).is_none() {
        eprintln!("ytt tools diagnose: failed to refresh tool selection");
        return 1;
    }

    let mut report = String::new();
    push_report_line(
        &mut report,
        format!("YuTuTui! {}", env!("CARGO_PKG_VERSION")),
    );
    push_report_line(&mut report, format!("target_os: {}", std::env::consts::OS));
    push_report_line(
        &mut report,
        format!("target_arch: {}", std::env::consts::ARCH),
    );
    push_report_line(
        &mut report,
        format!(
            "YTM_YTDLP: {}",
            active_env_ytdlp_override().unwrap_or_else(|| "<unset>".to_owned())
        ),
    );
    push_report_line(
        &mut report,
        format!(
            "config ytdlp_path: {}",
            cfg.tools
                .ytdlp_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unset>".to_owned())
        ),
    );
    push_report_line(
        &mut report,
        format!("managed enabled: {}", cfg.tools.managed_enabled()),
    );
    if let Some(error) = tools::ytdlp_selection_error() {
        push_report_line(&mut report, format!("selection error: {error}"));
    }
    match tools::ytdlp_selection() {
        Some(sel) => {
            push_report_line(
                &mut report,
                format!("selected source: {}", sel.source.label()),
            );
            push_report_line(
                &mut report,
                format!("selected path: {}", sel.path.display()),
            );
            push_report_line(
                &mut report,
                format!(
                    "selected version: {}",
                    sel.version.as_deref().unwrap_or("?")
                ),
            );
            if let Some(pin) = sel.pin_for_mpv() {
                push_report_line(&mut report, format!("mpv ytdl_path: {}", pin.display()));
            }
        }
        None => push_report_line(&mut report, "selected: none"),
    }

    let state = tools::ytdlp::load_state();
    push_report_line(
        &mut report,
        format!("managed metadata channel: {:?}", state.channel),
    );
    push_report_line(
        &mut report,
        format!(
            "managed metadata version: {}",
            state.version.as_deref().unwrap_or("?")
        ),
    );
    push_report_line(
        &mut report,
        format!(
            "managed metadata sha256: {}",
            state.sha256.as_deref().unwrap_or("?")
        ),
    );
    push_report_line(
        &mut report,
        format!(
            "managed metadata file: mtime={} len={}",
            state
                .installed_mtime_unix
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_owned()),
            state
                .installed_len
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_owned())
        ),
    );
    if let Some(path) = tools::ytdlp::installed_managed_path() {
        push_report_line(&mut report, format!("managed path: {}", path.display()));
        if let Some(actual) = inspect_ytdlp_path(&path) {
            push_report_line(
                &mut report,
                format!("managed actual version: {}", actual.version),
            );
            push_report_line(
                &mut report,
                format!("managed actual sha256: {}", actual.sha256),
            );
            push_report_line(
                &mut report,
                format!(
                    "managed actual file: mtime={} len={}",
                    actual.mtime_unix, actual.len
                ),
            );
        }
    }

    push_report_line(&mut report, "PATH candidates:");
    for (idx, path) in deps::resolve_all_on_path("yt-dlp").iter().enumerate() {
        let version = probe_ytdlp_path(path).unwrap_or_else(|| "?".to_owned());
        push_report_line(
            &mut report,
            format!("  {}. {} · {}", idx + 1, version, path.display()),
        );
    }
    push_report_line(&mut report, "JS runtimes:");
    for probe in tools::js_runtime_diagnostics() {
        push_report_line(
            &mut report,
            format!(
                "  {} path={} version={} supported={} reason={}",
                probe.runtime.label(),
                probe.path.display(),
                probe.version.as_deref().unwrap_or("?"),
                probe.supported,
                probe.reason.unwrap_or("")
            ),
        );
    }

    let Some(path) = diagnostic_path() else {
        eprintln!("ytt tools diagnose: no cache directory on this platform");
        return 1;
    };
    if let Some(dir) = path.parent()
        && let Err(e) = crate::util::safe_fs::ensure_private_dir(dir)
    {
        eprintln!(
            "ytt tools diagnose: failed to create {}: {e}",
            dir.display()
        );
        return 1;
    }
    if let Err(e) = crate::util::safe_fs::write_private_atomic(&path, report.as_bytes()) {
        eprintln!(
            "ytt tools diagnose: failed to write {}: {e}",
            path.display()
        );
        return 1;
    }
    println!(
        "{} {}",
        match lang {
            i18n::Language::Korean => "진단 파일:",
            i18n::Language::Japanese => "診断ファイル:",
            _ => "diagnostic file:",
        },
        path.display()
    );
    0
}

fn push_report_line(out: &mut String, line: impl AsRef<str>) {
    out.push_str(line.as_ref());
    out.push('\n');
}

fn inspect_ytdlp_path(path: &Path) -> Option<tools::ytdlp::BinaryInspection> {
    block_on(tools::ytdlp::inspect_binary(path)).and_then(Result::ok)
}

fn diagnostic_path() -> Option<PathBuf> {
    let ts = tools::ytdlp::now_unix();
    tools_cache_dir().map(|d| d.join("diagnostics").join(format!("tools-{ts}.txt")))
}

fn tools_cache_dir() -> Option<PathBuf> {
    crate::paths::cache_dir()
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

fn resolve_pin_target(target: &UseTarget, lang: i18n::Language) -> Result<ResolvedPin, String> {
    match target {
        UseTarget::System => {
            let path = deps::resolve_on_path("yt-dlp").ok_or_else(|| {
                match lang {
                    i18n::Language::Korean => "PATH에서 system yt-dlp를 찾지 못했습니다.",
                    i18n::Language::Japanese => "PATHにsystem yt-dlpが見つかりませんでした。",
                    _ => "system yt-dlp was not found on PATH.",
                }
                .to_owned()
            })?;
            let version = probe_ytdlp_path(&path).ok_or_else(|| match lang {
                i18n::Language::Korean => format!(
                    "system yt-dlp 버전을 확인할 수 없습니다: {}",
                    path.display()
                ),
                i18n::Language::Japanese => format!(
                    "system yt-dlpのバージョンを確認できません: {}",
                    path.display()
                ),
                _ => format!("could not read system yt-dlp version: {}", path.display()),
            })?;
            Ok(ResolvedPin {
                kind: PinKind::System,
                path,
                version,
            })
        }
        UseTarget::Managed => {
            let path = tools::ytdlp::installed_managed_path().ok_or_else(|| {
                match lang {
                    i18n::Language::Korean => {
                        "managed yt-dlp가 설치되어 있지 않습니다. 먼저 `ytt tools update`를 실행하세요."
                    }
                    i18n::Language::Japanese => {
                        "managed yt-dlpがインストールされていません。先に`ytt tools update`を実行してください。"
                    }
                    _ => "managed yt-dlp is not installed. Run `ytt tools update` first.",
                }
                .to_owned()
            })?;
            let version = probe_ytdlp_path(&path).ok_or_else(|| match lang {
                i18n::Language::Korean => format!(
                    "managed yt-dlp 버전을 확인할 수 없습니다: {}",
                    path.display()
                ),
                i18n::Language::Japanese => format!(
                    "managed yt-dlpのバージョンを確認できません: {}",
                    path.display()
                ),
                _ => format!("could not read managed yt-dlp version: {}", path.display()),
            })?;
            Ok(ResolvedPin {
                kind: PinKind::Managed,
                path,
                version,
            })
        }
        UseTarget::Path(raw_path) => {
            let path = std::fs::canonicalize(raw_path).map_err(|e| match lang {
                i18n::Language::Korean => format!(
                    "지정한 yt-dlp 경로를 열 수 없습니다: {} ({e})",
                    raw_path.display()
                ),
                i18n::Language::Japanese => format!(
                    "指定したyt-dlpのパスを開けません: {} ({e})",
                    raw_path.display()
                ),
                _ => format!("could not open yt-dlp path: {} ({e})", raw_path.display()),
            })?;
            let path_str = path.to_string_lossy();
            if !deps::on_path(path_str.as_ref()) {
                return Err(match lang {
                    i18n::Language::Korean => {
                        format!("지정한 yt-dlp가 실행 파일이 아닙니다: {}", path.display())
                    }
                    i18n::Language::Japanese => {
                        format!(
                            "指定したyt-dlpが実行ファイルではありません: {}",
                            path.display()
                        )
                    }
                    _ => format!("yt-dlp path is not executable: {}", path.display()),
                });
            }
            let version = probe_ytdlp_path(&path).ok_or_else(|| match lang {
                i18n::Language::Korean => format!(
                    "지정한 yt-dlp 버전을 확인할 수 없습니다: {}",
                    path.display()
                ),
                i18n::Language::Japanese => format!(
                    "指定したyt-dlpのバージョンを確認できません: {}",
                    path.display()
                ),
                _ => format!("could not read yt-dlp version: {}", path.display()),
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

fn warn_env_override(lang: i18n::Language) {
    let Some(value) = active_env_ytdlp_override() else {
        return;
    };

    eprintln!(
        "{}",
        match lang {
            i18n::Language::Korean => format!(
                "주의: 현재 프로세스에는 YTM_YTDLP={value} 가 설정되어 있어 config보다 우선합니다."
            ),
            i18n::Language::Japanese => format!(
                "注意: 現在のプロセスにはYTM_YTDLP={value}が設定されており、configより優先されます。"
            ),
            _ => format!("note: YTM_YTDLP={value} is set and still overrides config."),
        }
    );
}

fn active_env_ytdlp_override() -> Option<String> {
    std::env::var("YTM_YTDLP")
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn print_js_runtime_why(lang: i18n::Language) {
    let probes = tools::js_runtime_diagnostics();
    if let Some(probe) = probes.iter().find(|probe| probe.supported) {
        let version = probe
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let mode = if probe.runtime.flag_value().is_none() {
            match lang {
                i18n::Language::Korean => "자동 사용",
                i18n::Language::Japanese => "自動使用",
                _ => "auto-used",
            }
        } else {
            match lang {
                i18n::Language::Korean => "--js-runtimes 로 연결",
                i18n::Language::Japanese => "--js-runtimes で接続",
                _ => "wired via --js-runtimes",
            }
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
        println!(
            "  - JS runtime: {}",
            match lang {
                i18n::Language::Korean => "없음",
                i18n::Language::Japanese => "なし",
                _ => "none found",
            }
        );
    }
}

fn update() -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();

    let outcome = block_on(async {
        tools::init(&cfg.tools).await;
        tools::ytdlp::check_and_update(&cfg.tools, &|event| match event {
            tools::ToolsEvent::Progress {
                channel,
                percent: Some(p),
            } => println!("  … {p:>3}% ({})", channel.label()),
            tools::ToolsEvent::Progress { channel, .. } => println!(
                "{} ({})…",
                match lang {
                    i18n::Language::Korean => "yt-dlp 다운로드 중",
                    i18n::Language::Japanese => "yt-dlpをダウンロード中",
                    _ => "downloading yt-dlp",
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
                match lang {
                    i18n::Language::Korean => format!("yt-dlp {version} 설치 완료."),
                    i18n::Language::Japanese => format!("yt-dlp {version} インストール完了。"),
                    _ => format!("yt-dlp {version} installed."),
                }
            );
            0
        }
        tools::ytdlp::UpdateOutcome::AlreadyCurrent => {
            let state = tools::ytdlp::load_state();
            println!(
                "{}",
                match lang {
                    i18n::Language::Korean => format!(
                        "이미 최신입니다 ({}).",
                        state.version.as_deref().unwrap_or("?")
                    ),
                    i18n::Language::Japanese => format!(
                        "既に最新です ({})。",
                        state.version.as_deref().unwrap_or("?")
                    ),
                    _ => format!(
                        "Already up to date ({}).",
                        state.version.as_deref().unwrap_or("?")
                    ),
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
    use crate::test_util::env::with_var;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("yututui-tools-cli-{name}-{}", std::process::id()))
    }

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
    fn run_dispatch_handles_help_and_usage_errors_before_stateful_paths() {
        assert_eq!(run(&["--help".to_owned()]), 0);
        assert_eq!(run(&["help".to_owned()]), 0);
        assert_eq!(run(&["status".to_owned(), "--help".to_owned()]), 0);

        assert_eq!(run(&["unknown".to_owned()]), 2);
        assert_eq!(run(&["status".to_owned(), "--bogus".to_owned()]), 2);
        assert_eq!(run(&["use".to_owned()]), 2);
        assert_eq!(run(&["reset".to_owned()]), 2);
        assert_eq!(run(&["reset".to_owned(), "--not-playback".to_owned()]), 2);
    }

    #[test]
    fn parse_use_target_rejects_empty_and_keeps_path_shape() {
        assert_eq!(parse_use_target("   "), Err("target is empty"));
        assert_eq!(
            parse_use_target("yt-dlp"),
            Err("target must be `system`, `managed`, or an explicit path")
        );
        assert!(looks_like_path("/usr/local/bin/yt-dlp"));
        assert!(looks_like_path("../bin/yt-dlp"));
        assert!(looks_like_path(r"tools\yt-dlp.exe"));
        assert!(!looks_like_path("yt-dlp"));
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

    #[test]
    fn active_env_override_trims_and_ignores_empty_values() {
        with_var("YTM_YTDLP", None, || {
            assert_eq!(active_env_ytdlp_override(), None);
        });
        with_var("YTM_YTDLP", Some("   "), || {
            assert_eq!(active_env_ytdlp_override(), None);
        });
        with_var("YTM_YTDLP", Some("  /opt/bin/yt-dlp  "), || {
            assert_eq!(
                active_env_ytdlp_override().as_deref(),
                Some("/opt/bin/yt-dlp")
            );
        });
    }

    #[test]
    fn report_line_appends_exactly_one_newline() {
        let mut report = String::new();
        push_report_line(&mut report, "one");
        push_report_line(&mut report, String::from("two"));
        assert_eq!(report, "one\ntwo\n");
    }

    #[test]
    fn same_path_falls_back_to_literal_and_uses_canonical_files() {
        let path = temp_path("same-path-file");
        std::fs::write(&path, b"tool").unwrap();
        let same = path
            .parent()
            .unwrap()
            .join(".")
            .join(path.file_name().unwrap());

        assert!(same_path(&path, &same));
        assert!(same_path(
            Path::new("/definitely/not/present"),
            Path::new("/definitely/not/present")
        ));
        assert!(!same_path(
            Path::new("/definitely/not/present-a"),
            Path::new("/definitely/not/present-b")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn selection_label_uses_source_label_for_plain_sources() {
        let cfg = config::Config::default();
        for (source, label) in [
            (tools::YtdlpSource::Override, "override"),
            (tools::YtdlpSource::Managed, "managed"),
            (tools::YtdlpSource::System, "system"),
        ] {
            let sel = tools::YtdlpSelection {
                path: PathBuf::from(format!("/tmp/{label}-yt-dlp")),
                version: Some("2026.06.09".to_owned()),
                source,
            };
            assert_eq!(selection_label(&cfg, &sel), label);
        }
    }

    #[test]
    fn diagnostic_path_is_cache_scoped_and_timestamped() {
        let path = diagnostic_path().expect("project cache dir");
        assert_eq!(
            path.parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str()),
            Some("diagnostics")
        );
        assert!(path.to_string_lossy().contains("tools-"));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("txt"));
    }

    #[test]
    fn resolve_pin_target_reports_missing_system_when_path_is_empty() {
        with_var("PATH", Some(""), || {
            let err = resolve_pin_target(&UseTarget::System, i18n::Language::English).unwrap_err();
            assert!(err.contains("system yt-dlp was not found"));

            let err = resolve_pin_target(&UseTarget::System, i18n::Language::Korean).unwrap_err();
            assert!(err.contains("system yt-dlp"));
        });
    }

    #[test]
    fn resolve_pin_target_rejects_missing_explicit_path() {
        let missing = temp_path("missing-path");
        let _ = std::fs::remove_file(&missing);

        let err = resolve_pin_target(&UseTarget::Path(missing.clone()), i18n::Language::English)
            .unwrap_err();
        assert!(err.contains("could not open yt-dlp path"));
        assert!(err.contains(&missing.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_pin_target_rejects_non_executable_explicit_path() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("non-executable");
        std::fs::write(&path, b"not executable").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = resolve_pin_target(&UseTarget::Path(path.clone()), i18n::Language::English)
            .unwrap_err();
        assert!(err.contains("yt-dlp path is not executable"));

        let _ = std::fs::remove_file(path);
    }
}
